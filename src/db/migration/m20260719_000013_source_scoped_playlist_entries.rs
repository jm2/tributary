//! Migration: make regular-playlist identity source scoped.
//!
//! `track_id` remains the durable source-native identity and is now paired
//! with a canonical `source_id`. `local_track_id` is a separate nullable
//! foreign-key binding used only for the built-in local source. Keeping the
//! binding separate means a local deletion can make an occurrence unavailable
//! without erasing its durable identity, while non-local IDs are never
//! misinterpreted as keys in the local `tracks` table.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

/// Frozen value of `SourceId::local()` at the migration boundary.
///
/// This is persistent format state. It must not follow a future identity
/// migration implicitly.
const LOCAL_SOURCE_ID: &str = "dbae1f16-7921-5209-939e-ce3177ec7b57";
const NIL_SOURCE_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Frozen Unicode `White_Space` code points used by Rust's `str::trim`.
///
/// SQLite's one-argument `trim` only removes ASCII spaces. Listing the same
/// characters explicitly keeps database constraints aligned with playlist
/// decoding and import validation, both of which use Rust `trim()`.
const RUST_TRIM_WHITESPACE: &[u32] = &[
    9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200,
    8201, 8202, 8232, 8233, 8239, 8287, 12288,
];

const REBUILD_TABLE: &str = "tributary_playlist_entries_source_scope_rebuild";
const PLAYLIST_INDEX: &str = "idx_playlist_entries_playlist_id";
const LEGACY_TRACK_INDEX: &str = "idx_playlist_entries_track_id";
const SOURCE_TRACK_INDEX: &str = "idx_playlist_entries_source_track_id";
const LOCAL_TRACK_INDEX: &str = "idx_playlist_entries_local_track_id";
const UNIQUE_POSITION_INDEX: &str = "idx_playlist_entries_playlist_position_unique";

const SOURCE_ID_CHECK: &str = "ck_playlist_entries_source_id";
const TRACK_ID_CHECK: &str = "ck_playlist_entries_track_id";
const LOCAL_BINDING_CHECK: &str = "ck_playlist_entries_local_binding";
const MATCH_PATH_CHECK: &str = "ck_playlist_entries_match_path";
const ORPHAN_EVIDENCE_CHECK: &str = "ck_playlist_entries_orphan_evidence";
const POSITION_CHECK: &str = "ck_playlist_entries_position";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistEntriesSchema {
    LegacyLocal,
    SourceScoped,
}

#[derive(Clone, Debug)]
struct ExplicitIndex {
    name: String,
    sql: String,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_playlist_entries(manager, PlaylistEntriesSchema::SourceScoped).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_playlist_entries(manager, PlaylistEntriesSchema::LegacyLocal).await
    }
}

/// Own the complete rebuild transaction because SQLite/SeaORM does not wrap
/// this migration automatically. A failed copy, constraint, index recreation,
/// or validation therefore restores both the original schema and every row.
async fn migrate_playlist_entries(
    manager: &SchemaManager<'_>,
    target: PlaylistEntriesSchema,
) -> Result<(), DbErr> {
    if !manager.has_table("playlist_entries").await? {
        return Err(DbErr::Migration(
            "playlist_entries must exist for source-scoped identity migration".to_string(),
        ));
    }

    let transaction = manager.get_connection().begin().await?;
    let result = {
        let manager = SchemaManager::new(&transaction);
        let current = inspect_schema(&manager).await?;
        validate_schema(&manager, current).await?;

        if current == target {
            Ok(())
        } else {
            if target == PlaylistEntriesSchema::LegacyLocal {
                require_lossless_legacy_downgrade(&manager).await?;
            }
            rebuild(&manager, current, target).await
        }
    };

    match result {
        Ok(()) => transaction.commit().await,
        Err(error) => {
            let rollback_result = transaction.rollback().await;
            Err(preserve_original_error(error, rollback_result))
        }
    }
}

/// Never replace the unsafe migration failure with a secondary rollback
/// failure. If rollback also fails, retain both errors in the returned context.
fn preserve_original_error(original: DbErr, rollback_result: Result<(), DbErr>) -> DbErr {
    match rollback_result {
        Ok(()) => original,
        Err(rollback_error) => DbErr::Migration(format!(
            "{original}; additionally failed to roll back source-scoped playlist migration: \
             {rollback_error}"
        )),
    }
}

async fn rebuild(
    manager: &SchemaManager<'_>,
    current: PlaylistEntriesSchema,
    target: PlaylistEntriesSchema,
) -> Result<(), DbErr> {
    let custom_indexes = capture_custom_indexes(manager, current).await?;
    let connection = manager.get_connection();

    let create_sql = match target {
        PlaylistEntriesSchema::LegacyLocal => legacy_table_sql(REBUILD_TABLE),
        PlaylistEntriesSchema::SourceScoped => scoped_table_sql(REBUILD_TABLE),
    };
    connection.execute_unprepared(&create_sql).await?;

    match target {
        PlaylistEntriesSchema::SourceScoped => {
            connection
                .execute_unprepared(&format!(
                    "INSERT INTO {REBUILD_TABLE} (
                         id, playlist_id, position, source_id, track_id, local_track_id,
                         match_title, match_artist, match_album, match_duration_secs,
                         match_file_path
                     )
                     SELECT id, playlist_id, position, '{LOCAL_SOURCE_ID}', track_id, track_id,
                            match_title, match_artist, match_album, match_duration_secs,
                            match_file_path
                     FROM playlist_entries"
                ))
                .await?;
        }
        PlaylistEntriesSchema::LegacyLocal => {
            connection
                .execute_unprepared(&format!(
                    "INSERT INTO {REBUILD_TABLE} (
                         id, playlist_id, position, track_id,
                         match_title, match_artist, match_album, match_duration_secs,
                         match_file_path
                     )
                     SELECT id, playlist_id, position, track_id,
                            match_title, match_artist, match_album, match_duration_secs,
                            match_file_path
                     FROM playlist_entries"
                ))
                .await?;
        }
    }

    connection
        .execute_unprepared("DROP TABLE playlist_entries")
        .await?;
    connection
        .execute_unprepared(&format!(
            "ALTER TABLE {REBUILD_TABLE} RENAME TO playlist_entries"
        ))
        .await?;

    create_standard_indexes(manager, target).await?;
    for index in custom_indexes {
        connection
            .execute_unprepared(&index.sql)
            .await
            .map_err(|error| {
                DbErr::Migration(format!(
                    "failed to restore playlist_entries index {}: {error}",
                    index.name
                ))
            })?;
    }

    validate_schema(manager, target).await
}

fn legacy_table_sql(table: &str) -> String {
    format!(
        "CREATE TABLE {table} (
             id VARCHAR PRIMARY KEY NOT NULL,
             playlist_id VARCHAR NOT NULL,
             position INTEGER NOT NULL,
             track_id VARCHAR NULL,
             match_title VARCHAR NOT NULL DEFAULT '',
             match_artist VARCHAR NOT NULL DEFAULT '',
             match_album VARCHAR NOT NULL DEFAULT '',
             match_duration_secs INTEGER NULL,
             match_file_path VARCHAR NULL,
             CONSTRAINT fk_entry_playlist
                 FOREIGN KEY (playlist_id)
                 REFERENCES playlists (id)
                 ON DELETE CASCADE,
             CONSTRAINT fk_entry_track
                 FOREIGN KEY (track_id)
                 REFERENCES tracks (id)
                 ON DELETE SET NULL
         )"
    )
}

fn scoped_table_sql(table: &str) -> String {
    let trim_characters = RUST_TRIM_WHITESPACE
        .iter()
        .map(|code_point| {
            if *code_point == 32 {
                "' '".to_string()
            } else {
                format!("char({code_point})")
            }
        })
        .collect::<Vec<_>>()
        .join(" || ");
    format!(
        "CREATE TABLE {table} (
             id VARCHAR PRIMARY KEY NOT NULL,
             playlist_id VARCHAR NOT NULL,
             position INTEGER NOT NULL,
             source_id VARCHAR NOT NULL,
             track_id VARCHAR NULL,
             local_track_id VARCHAR NULL,
             match_title VARCHAR NOT NULL DEFAULT '',
             match_artist VARCHAR NOT NULL DEFAULT '',
             match_album VARCHAR NOT NULL DEFAULT '',
             match_duration_secs INTEGER NULL,
             match_file_path VARCHAR NULL,
             CONSTRAINT {SOURCE_ID_CHECK} CHECK (
                 length(CAST(source_id AS BLOB)) = 36
                 AND source_id = lower(source_id)
                 AND substr(source_id, 9, 1) = '-'
                 AND substr(source_id, 14, 1) = '-'
                 AND substr(source_id, 19, 1) = '-'
                 AND substr(source_id, 24, 1) = '-'
                 AND length(replace(source_id, '-', '')) = 32
                 AND replace(source_id, '-', '') NOT GLOB '*[^0-9a-f]*'
                 AND source_id <> '{NIL_SOURCE_ID}'
             ),
             CONSTRAINT {TRACK_ID_CHECK} CHECK (
                 (source_id = '{LOCAL_SOURCE_ID}' AND (
                     track_id IS NULL OR length(CAST(track_id AS BLOB)) BETWEEN 1 AND 262144
                 ))
                 OR
                 (source_id <> '{LOCAL_SOURCE_ID}' AND
                     track_id IS NOT NULL AND
                     length(CAST(track_id AS BLOB)) BETWEEN 1 AND 4096
                 )
             ),
             CONSTRAINT {LOCAL_BINDING_CHECK} CHECK (
                 (source_id = '{LOCAL_SOURCE_ID}' AND (
                     local_track_id IS NULL OR
                     (track_id IS NOT NULL AND local_track_id = track_id)
                 ))
                 OR
                 (source_id <> '{LOCAL_SOURCE_ID}' AND local_track_id IS NULL)
             ),
             CONSTRAINT {MATCH_PATH_CHECK} CHECK (
                 source_id = '{LOCAL_SOURCE_ID}' OR match_file_path IS NULL
             ),
             CONSTRAINT {ORPHAN_EVIDENCE_CHECK} CHECK (
                 track_id IS NOT NULL OR (
                     source_id = '{LOCAL_SOURCE_ID}' AND
                     (
                         (
                             match_file_path IS NOT NULL AND
                             length(CAST(trim(match_file_path, {trim_characters}) AS BLOB)) > 0
                         )
                         OR
                         (
                             length(CAST(trim(match_title, {trim_characters}) AS BLOB)) > 0 AND
                             length(CAST(trim(match_artist, {trim_characters}) AS BLOB)) > 0
                         )
                     )
                 )
             ),
             CONSTRAINT {POSITION_CHECK} CHECK (position >= 0),
             CONSTRAINT fk_entry_playlist
                 FOREIGN KEY (playlist_id)
                 REFERENCES playlists (id)
                 ON DELETE CASCADE,
             CONSTRAINT fk_entry_local_track
                 FOREIGN KEY (local_track_id)
                 REFERENCES tracks (id)
                 ON DELETE SET NULL
         )"
    )
}

async fn inspect_schema(manager: &SchemaManager<'_>) -> Result<PlaylistEntriesSchema, DbErr> {
    let columns = table_columns(manager).await?;
    if columns == legacy_columns() {
        let actual_sql = table_sql(manager).await?;
        let expected_sql = legacy_table_sql("playlist_entries");
        if canonical_sql(&actual_sql) != canonical_sql(&expected_sql) {
            return Err(DbErr::Migration(format!(
                "legacy playlist_entries does not match the exact migration-12 predecessor: \
                 {actual_sql}"
            )));
        }
        return Ok(PlaylistEntriesSchema::LegacyLocal);
    }
    if columns == scoped_columns() {
        let actual_sql = table_sql(manager).await?;
        let expected_sql = scoped_table_sql("playlist_entries");
        if canonical_sql(&actual_sql) != canonical_sql(&expected_sql) {
            return Err(DbErr::Migration(format!(
                "source-scoped playlist_entries has unexpected table SQL: {actual_sql}"
            )));
        }
        return Ok(PlaylistEntriesSchema::SourceScoped);
    }
    Err(DbErr::Migration(format!(
        "playlist_entries has an unexpected column schema: {columns:?}"
    )))
}

type ColumnSchema = (String, String, i32, Option<String>, i32);

async fn table_columns(manager: &SchemaManager<'_>) -> Result<Vec<ColumnSchema>, DbErr> {
    manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA table_info('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "name")?,
                row.try_get::<String>("", "type")?.to_ascii_lowercase(),
                row.try_get("", "notnull")?,
                row.try_get("", "dflt_value")?,
                row.try_get("", "pk")?,
            ))
        })
        .collect()
}

fn legacy_columns() -> Vec<ColumnSchema> {
    vec![
        column("id", "varchar", 1, None, 1),
        column("playlist_id", "varchar", 1, None, 0),
        column("position", "integer", 1, None, 0),
        column("track_id", "varchar", 0, None, 0),
        column("match_title", "varchar", 1, Some("''"), 0),
        column("match_artist", "varchar", 1, Some("''"), 0),
        column("match_album", "varchar", 1, Some("''"), 0),
        column("match_duration_secs", "integer", 0, None, 0),
        column("match_file_path", "varchar", 0, None, 0),
    ]
}

fn scoped_columns() -> Vec<ColumnSchema> {
    vec![
        column("id", "varchar", 1, None, 1),
        column("playlist_id", "varchar", 1, None, 0),
        column("position", "integer", 1, None, 0),
        column("source_id", "varchar", 1, None, 0),
        column("track_id", "varchar", 0, None, 0),
        column("local_track_id", "varchar", 0, None, 0),
        column("match_title", "varchar", 1, Some("''"), 0),
        column("match_artist", "varchar", 1, Some("''"), 0),
        column("match_album", "varchar", 1, Some("''"), 0),
        column("match_duration_secs", "integer", 0, None, 0),
        column("match_file_path", "varchar", 0, None, 0),
    ]
}

fn column(
    name: &str,
    sql_type: &str,
    not_null: i32,
    default: Option<&str>,
    primary_key: i32,
) -> ColumnSchema {
    (
        name.to_string(),
        sql_type.to_string(),
        not_null,
        default.map(str::to_string),
        primary_key,
    )
}

async fn table_sql(manager: &SchemaManager<'_>) -> Result<String, DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'playlist_entries'"
                .to_string(),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("playlist_entries SQL is missing".to_string()))?;
    row.try_get("", "sql")
}

fn canonical_sql(sql: &str) -> String {
    let mut canonical = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                // String contents are semantically significant. Preserve
                // whitespace, case, delimiters, and doubled-quote escapes.
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
                // SQLite accepts double-quoted strings when DQS compatibility
                // is enabled. Only normalize tokens known to be identifiers in
                // these frozen schemas; preserve every other token literally.
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
                if is_known_schema_identifier(&token) {
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

fn is_known_schema_identifier(identifier: &str) -> bool {
    matches!(
        identifier.to_ascii_lowercase().as_str(),
        "playlist_entries"
            | "id"
            | "playlist_id"
            | "position"
            | "source_id"
            | "track_id"
            | "local_track_id"
            | "match_title"
            | "match_artist"
            | "match_album"
            | "match_duration_secs"
            | "match_file_path"
            | "playlists"
            | "tracks"
            | "fk_entry_playlist"
            | "fk_entry_track"
            | "fk_entry_local_track"
            | SOURCE_ID_CHECK
            | TRACK_ID_CHECK
            | LOCAL_BINDING_CHECK
            | MATCH_PATH_CHECK
            | ORPHAN_EVIDENCE_CHECK
            | POSITION_CHECK
            | PLAYLIST_INDEX
            | LEGACY_TRACK_INDEX
            | SOURCE_TRACK_INDEX
            | LOCAL_TRACK_INDEX
            | UNIQUE_POSITION_INDEX
    )
}

async fn validate_schema(
    manager: &SchemaManager<'_>,
    schema: PlaylistEntriesSchema,
) -> Result<(), DbErr> {
    validate_foreign_keys(manager, schema).await?;
    validate_standard_indexes(manager, schema).await?;
    validate_foreign_key_rows(manager).await
}

async fn validate_foreign_keys(
    manager: &SchemaManager<'_>,
    schema: PlaylistEntriesSchema,
) -> Result<(), DbErr> {
    let mut actual = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA foreign_key_list('playlist_entries')".to_string(),
        ))
        .await?
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
    actual.sort();

    let track_column = match schema {
        PlaylistEntriesSchema::LegacyLocal => "track_id",
        PlaylistEntriesSchema::SourceScoped => "local_track_id",
    };
    let mut expected = vec![
        (
            "playlist_id".to_string(),
            "playlists".to_string(),
            "id".to_string(),
            "NO ACTION".to_string(),
            "CASCADE".to_string(),
        ),
        (
            track_column.to_string(),
            "tracks".to_string(),
            "id".to_string(),
            "NO ACTION".to_string(),
            "SET NULL".to_string(),
        ),
    ];
    expected.sort();
    if actual != expected {
        return Err(DbErr::Migration(format!(
            "playlist_entries has unexpected foreign keys: {actual:?}"
        )));
    }
    Ok(())
}

async fn validate_foreign_key_rows(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let violations = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA foreign_key_check('playlist_entries')".to_string(),
        ))
        .await?;
    if !violations.is_empty() {
        return Err(DbErr::Migration(format!(
            "playlist_entries has {} foreign-key violation(s)",
            violations.len()
        )));
    }
    Ok(())
}

async fn capture_custom_indexes(
    manager: &SchemaManager<'_>,
    schema: PlaylistEntriesSchema,
) -> Result<Vec<ExplicitIndex>, DbErr> {
    validate_standard_indexes(manager, schema).await?;
    let standard = standard_index_names(schema);
    manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "SELECT name, sql FROM sqlite_master
             WHERE type = 'index'
               AND tbl_name = 'playlist_entries'
               AND sql IS NOT NULL
             ORDER BY name"
                .to_string(),
        ))
        .await?
        .into_iter()
        .filter_map(|row| {
            let name = row.try_get::<String>("", "name");
            let sql = row.try_get::<String>("", "sql");
            match (name, sql) {
                (Ok(name), Ok(sql)) if !standard.contains(&name.as_str()) => {
                    Some(Ok(ExplicitIndex { name, sql }))
                }
                (Ok(_), Ok(_)) => None,
                (Err(error), _) | (_, Err(error)) => Some(Err(error)),
            }
        })
        .collect()
}

fn standard_index_names(schema: PlaylistEntriesSchema) -> &'static [&'static str] {
    match schema {
        PlaylistEntriesSchema::LegacyLocal => {
            &[PLAYLIST_INDEX, LEGACY_TRACK_INDEX, UNIQUE_POSITION_INDEX]
        }
        PlaylistEntriesSchema::SourceScoped => &[
            PLAYLIST_INDEX,
            SOURCE_TRACK_INDEX,
            LOCAL_TRACK_INDEX,
            UNIQUE_POSITION_INDEX,
        ],
    }
}

async fn create_standard_indexes(
    manager: &SchemaManager<'_>,
    schema: PlaylistEntriesSchema,
) -> Result<(), DbErr> {
    let connection = manager.get_connection();
    connection
        .execute_unprepared(&format!(
            "CREATE INDEX {PLAYLIST_INDEX} ON playlist_entries (playlist_id)"
        ))
        .await?;
    match schema {
        PlaylistEntriesSchema::LegacyLocal => {
            connection
                .execute_unprepared(&format!(
                    "CREATE INDEX {LEGACY_TRACK_INDEX} ON playlist_entries (track_id)"
                ))
                .await?;
        }
        PlaylistEntriesSchema::SourceScoped => {
            connection
                .execute_unprepared(&format!(
                    "CREATE INDEX {SOURCE_TRACK_INDEX}
                     ON playlist_entries (source_id, track_id)"
                ))
                .await?;
            connection
                .execute_unprepared(&format!(
                    "CREATE INDEX {LOCAL_TRACK_INDEX} ON playlist_entries (local_track_id)"
                ))
                .await?;
        }
    }
    connection
        .execute_unprepared(&format!(
            "CREATE UNIQUE INDEX {UNIQUE_POSITION_INDEX}
             ON playlist_entries (playlist_id, position)"
        ))
        .await?;
    Ok(())
}

async fn validate_standard_indexes(
    manager: &SchemaManager<'_>,
    schema: PlaylistEntriesSchema,
) -> Result<(), DbErr> {
    validate_primary_key_index(manager).await?;
    validate_index(manager, PLAYLIST_INDEX, false, &["playlist_id"]).await?;
    validate_index(
        manager,
        UNIQUE_POSITION_INDEX,
        true,
        &["playlist_id", "position"],
    )
    .await?;
    match schema {
        PlaylistEntriesSchema::LegacyLocal => {
            validate_index(manager, LEGACY_TRACK_INDEX, false, &["track_id"]).await?;
        }
        PlaylistEntriesSchema::SourceScoped => {
            validate_index(
                manager,
                SOURCE_TRACK_INDEX,
                false,
                &["source_id", "track_id"],
            )
            .await?;
            validate_index(manager, LOCAL_TRACK_INDEX, false, &["local_track_id"]).await?;
        }
    }
    Ok(())
}

async fn validate_index(
    manager: &SchemaManager<'_>,
    name: &str,
    expected_unique: bool,
    expected_columns: &[&str],
) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA index_list('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .find(|row| {
            row.try_get::<String>("", "name")
                .is_ok_and(|value| value == name)
        })
        .ok_or_else(|| DbErr::Migration(format!("playlist_entries is missing index {name}")))?;
    let unique = row.try_get::<i32>("", "unique")? == 1;
    let origin: String = row.try_get("", "origin")?;
    let partial = row.try_get::<i32>("", "partial")? == 1;
    if unique != expected_unique || origin != "c" || partial {
        return Err(DbErr::Migration(format!(
            "playlist_entries index {name} has unique={unique}, origin={origin}, \
             partial={partial}"
        )));
    }

    let actual_sql = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master
             WHERE type = 'index' AND name = ? AND tbl_name = 'playlist_entries'",
            [name.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("playlist_entries index {name} SQL is missing")))?
        .try_get::<String>("", "sql")?;
    let expected_sql = format!(
        "CREATE {}INDEX {name} ON playlist_entries ({})",
        if expected_unique { "UNIQUE " } else { "" },
        expected_columns.join(", ")
    );
    if canonical_sql(&actual_sql) != canonical_sql(&expected_sql) {
        return Err(DbErr::Migration(format!(
            "playlist_entries index {name} has unexpected SQL: {actual_sql}"
        )));
    }

    let quoted = name.replace('\'', "''");
    let mut columns = manager
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
    columns.sort_by_key(|(sequence, _)| *sequence);
    let columns: Vec<_> = columns.into_iter().map(|(_, column)| column).collect();
    let expected: Vec<_> = expected_columns
        .iter()
        .map(|column| (*column).to_string())
        .collect();
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "playlist_entries index {name} has columns {columns:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

async fn validate_primary_key_index(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let implicit = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA index_list('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .filter_map(|row| {
            let origin = row.try_get::<String>("", "origin").ok()?;
            (origin != "c").then(|| {
                Ok::<_, DbErr>((
                    row.try_get::<String>("", "name")?,
                    row.try_get::<i32>("", "unique")? == 1,
                    origin,
                    row.try_get::<i32>("", "partial")? == 1,
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [(name, true, origin, false)] = implicit.as_slice() else {
        return Err(DbErr::Migration(format!(
            "playlist_entries has unexpected implicit indexes: {implicit:?}"
        )));
    };
    if origin != "pk" {
        return Err(DbErr::Migration(format!(
            "playlist_entries primary-key index {name} has origin {origin}"
        )));
    }

    let quoted = name.replace('\'', "''");
    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_info('{quoted}')"),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    if columns != ["id"] {
        return Err(DbErr::Migration(format!(
            "playlist_entries primary-key index has columns {columns:?}"
        )));
    }
    Ok(())
}

async fn require_lossless_legacy_downgrade(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT COUNT(*) AS count
             FROM playlist_entries
             WHERE source_id <> ?
                OR track_id IS NOT local_track_id",
            [LOCAL_SOURCE_ID.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("failed to inspect playlist entries".to_string()))?;
    let count: i64 = row.try_get("", "count")?;
    if count != 0 {
        return Err(DbErr::Migration(format!(
            "cannot downgrade {count} source-scoped playlist entry row(s) losslessly"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, ExecResult, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    const REMOTE_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND_REMOTE_SOURCE_ID: &str = "22222222-2222-4222-8222-222222222222";

    async fn database_before_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        Migrator::up(&db, Some(12))
            .await
            .expect("apply migrations preceding source-scoped playlists");
        db
    }

    async fn insert_playlist(db: &DatabaseConnection, id: &str) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES (?, ?, '2026-07-19T00:00:00Z', '2026-07-19T00:00:00Z')",
            [id.into(), format!("Playlist {id}").into()],
        ))
        .await
        .expect("insert playlist");
    }

    async fn insert_track(db: &DatabaseConnection, id: &str) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO tracks (
                 id, file_path, title, artist_name, album_title,
                 date_added, date_modified
             )
             VALUES (?, ?, ?, 'Artist', 'Album',
                     '2026-07-19T00:00:00Z', '2026-07-19T00:00:00Z')",
            [
                id.into(),
                format!("/music/{id}.flac").into(),
                format!("Track {id}").into(),
            ],
        ))
        .await
        .expect("insert track");
    }

    async fn insert_legacy_entry(
        db: &DatabaseConnection,
        id: &str,
        playlist_id: &str,
        position: i32,
        track_id: Option<&str>,
        path: Option<&str>,
    ) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, track_id,
                 match_title, match_artist, match_album, match_duration_secs,
                 match_file_path
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            [
                id.into(),
                playlist_id.into(),
                position.into(),
                track_id.into(),
                format!("title-{id}").into(),
                format!("artist-{id}").into(),
                format!("album-{id}").into(),
                (position + 180).into(),
                path.into(),
            ],
        ))
        .await
        .expect("insert legacy playlist entry");
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_scoped_entry(
        db: &DatabaseConnection,
        id: &str,
        playlist_id: &str,
        position: i32,
        source_id: &str,
        track_id: Option<&str>,
        local_track_id: Option<&str>,
        path: Option<&str>,
    ) -> Result<ExecResult, DbErr> {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs,
                 match_file_path
             )
             VALUES (?, ?, ?, ?, ?, ?, 'title', 'artist', 'album', 180, ?)",
            [
                id.into(),
                playlist_id.into(),
                position.into(),
                source_id.into(),
                track_id.into(),
                local_track_id.into(),
                path.into(),
            ],
        ))
        .await
    }

    type ScopedRow = (
        String,
        String,
        i32,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        Option<i32>,
        Option<String>,
    );

    async fn scoped_rows(db: &DatabaseConnection) -> Vec<ScopedRow> {
        db.query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT id, playlist_id, position, source_id, track_id, local_track_id,
                    match_title, match_artist, match_album, match_duration_secs,
                    match_file_path
             FROM playlist_entries
             ORDER BY playlist_id, position"
                .to_string(),
        ))
        .await
        .expect("query scoped entries")
        .into_iter()
        .map(|row| {
            (
                row.try_get("", "id").expect("id"),
                row.try_get("", "playlist_id").expect("playlist"),
                row.try_get("", "position").expect("position"),
                row.try_get("", "source_id").expect("source"),
                row.try_get("", "track_id").expect("track"),
                row.try_get("", "local_track_id").expect("local track"),
                row.try_get("", "match_title").expect("title"),
                row.try_get("", "match_artist").expect("artist"),
                row.try_get("", "match_album").expect("album"),
                row.try_get("", "match_duration_secs").expect("duration"),
                row.try_get("", "match_file_path").expect("path"),
            )
        })
        .collect()
    }

    async fn row_count(db: &DatabaseConnection, predicate: &str) -> i64 {
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM playlist_entries WHERE {predicate}"),
            ))
            .await
            .expect("count entries")
            .expect("count row");
        row.try_get("", "count").expect("count")
    }

    async fn has_index(db: &DatabaseConnection, name: &str) -> bool {
        db.query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT 1 AS present FROM sqlite_master
             WHERE type = 'index' AND name = ? AND tbl_name = 'playlist_entries'",
            [name.into()],
        ))
        .await
        .expect("query index")
        .is_some()
    }

    async fn migration_table_exists(db: &DatabaseConnection) -> bool {
        db.query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT 1 AS present FROM sqlite_master WHERE type = 'table' AND name = ?",
            [REBUILD_TABLE.into()],
        ))
        .await
        .expect("query rebuild table")
        .is_some()
    }

    async fn install_near_match_legacy_schema(db: &DatabaseConnection) {
        let near_table = "near_match_playlist_entries";
        let near_sql = legacy_table_sql(near_table).replace(
            "position INTEGER NOT NULL,",
            "position INTEGER NOT NULL CONSTRAINT unexpected_position_check \
             CHECK (position >= 0),",
        );
        db.execute_unprepared(&near_sql)
            .await
            .expect("create near-match legacy table");
        db.execute_unprepared(&format!(
            "INSERT INTO {near_table} (
                 id, playlist_id, position, track_id,
                 match_title, match_artist, match_album, match_duration_secs,
                 match_file_path
             )
             SELECT id, playlist_id, position, track_id,
                    match_title, match_artist, match_album, match_duration_secs,
                    match_file_path
             FROM playlist_entries"
        ))
        .await
        .expect("copy near-match legacy rows");
        db.execute_unprepared("DROP TABLE playlist_entries")
            .await
            .expect("drop exact predecessor");
        db.execute_unprepared(&format!(
            "ALTER TABLE {near_table} RENAME TO playlist_entries"
        ))
        .await
        .expect("install near-match legacy table");
        create_standard_indexes(&SchemaManager::new(db), PlaylistEntriesSchema::LegacyLocal)
            .await
            .expect("restore legacy indexes");
    }

    async fn install_near_match_scoped_schema(db: &DatabaseConnection, near_sql: &str) {
        let near_table = "near_match_playlist_entries";
        db.execute_unprepared(near_sql)
            .await
            .expect("create near-match scoped table");
        db.execute_unprepared(&format!(
            "INSERT INTO {near_table} (
                 id, playlist_id, position, source_id, track_id, local_track_id,
                 match_title, match_artist, match_album, match_duration_secs,
                 match_file_path
             )
             SELECT id, playlist_id, position, source_id, track_id, local_track_id,
                    match_title, match_artist, match_album, match_duration_secs,
                    match_file_path
             FROM playlist_entries"
        ))
        .await
        .expect("copy near-match scoped rows");
        db.execute_unprepared("DROP TABLE playlist_entries")
            .await
            .expect("drop exact scoped table");
        db.execute_unprepared(&format!(
            "ALTER TABLE {near_table} RENAME TO playlist_entries"
        ))
        .await
        .expect("install near-match scoped table");
        create_standard_indexes(&SchemaManager::new(db), PlaylistEntriesSchema::SourceScoped)
            .await
            .expect("restore scoped indexes");
    }

    #[test]
    fn frozen_migration_local_source_id_matches_the_architecture_constant() {
        assert_eq!(
            LOCAL_SOURCE_ID,
            crate::architecture::SourceId::local().to_string()
        );
    }

    #[test]
    fn frozen_sql_trim_set_matches_rust_trim_whitespace() {
        let rust_whitespace = (0..=char::MAX as u32)
            .filter_map(char::from_u32)
            .filter(|character| character.is_whitespace())
            .map(u32::from)
            .collect::<Vec<_>>();

        assert_eq!(RUST_TRIM_WHITESPACE, rust_whitespace);
    }

    #[test]
    fn canonical_sql_preserves_literals_and_only_normalizes_known_identifier_quotes() {
        assert_eq!(
            canonical_sql(r#"CREATE TABLE "playlist_entries" ("source_id" VARCHAR DEFAULT '')"#),
            canonical_sql("CREATE TABLE playlist_entries (source_id VARCHAR DEFAULT '')")
        );
        assert_ne!(
            canonical_sql(&format!("CHECK (source_id = '{LOCAL_SOURCE_ID}')")),
            canonical_sql(&format!(
                "CHECK (source_id = '{}')",
                LOCAL_SOURCE_ID.to_ascii_uppercase()
            ))
        );
        assert_ne!(
            canonical_sql(&format!("CHECK (source_id = '{LOCAL_SOURCE_ID}')")),
            canonical_sql(&format!(r#"CHECK (source_id = "{LOCAL_SOURCE_ID}")"#))
        );
        assert_ne!(
            canonical_sql("CHECK (trim(value, ' ') <> '')"),
            canonical_sql("CHECK (trim(value, '') <> '')")
        );
    }

    #[test]
    fn rollback_failure_retains_the_original_and_secondary_context() {
        let original = DbErr::Migration("forced rebuild failure".to_string());
        let returned = preserve_original_error(
            original,
            Err(DbErr::Migration("forced rollback failure".to_string())),
        );
        let message = returned.to_string();

        assert!(message.contains("forced rebuild failure"));
        assert!(message.contains("forced rollback failure"));
        assert_eq!(
            preserve_original_error(DbErr::Migration("only original".to_string()), Ok(()))
                .to_string(),
            "Migration Error: only original"
        );
    }

    #[tokio::test]
    async fn upgrade_backfills_exact_local_identity_and_preserves_occurrences_and_evidence() {
        let db = database_before_migration().await;
        insert_playlist(&db, "a").await;
        insert_playlist(&db, "b").await;
        insert_track(&db, "shared").await;
        insert_track(&db, "other").await;
        insert_legacy_entry(
            &db,
            "entry-a0",
            "a",
            0,
            Some("shared"),
            Some("/import/shared.flac"),
        )
        .await;
        insert_legacy_entry(&db, "entry-a1", "a", 1, Some("shared"), None).await;
        insert_legacy_entry(&db, "entry-a2", "a", 2, None, Some("/import/future.flac")).await;
        insert_legacy_entry(&db, "entry-b0", "b", 0, Some("other"), None).await;
        db.execute_unprepared(
            "CREATE INDEX idx_playlist_entries_match_title_custom
             ON playlist_entries (match_title)
             WHERE match_title <> ''",
        )
        .await
        .expect("create custom index");

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade source-scoped entries");
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("repeat upgrade validates in place");

        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::SourceScoped)
        );
        let rows = scoped_rows(&db).await;
        assert_eq!(rows.len(), 4);
        assert_eq!(&rows[0].0, "entry-a0");
        assert_eq!(rows[0].2, 0);
        assert_eq!(&rows[0].3, LOCAL_SOURCE_ID);
        assert_eq!(rows[0].4.as_deref(), Some("shared"));
        assert_eq!(rows[0].5.as_deref(), Some("shared"));
        assert_eq!(rows[0].6, "title-entry-a0");
        assert_eq!(rows[0].7, "artist-entry-a0");
        assert_eq!(rows[0].8, "album-entry-a0");
        assert_eq!(rows[0].9, Some(180));
        assert_eq!(rows[0].10.as_deref(), Some("/import/shared.flac"));
        assert_eq!(&rows[1].0, "entry-a1");
        assert_eq!(rows[1].4, rows[0].4, "duplicate media pair is retained");
        assert_eq!(rows[1].5, rows[0].5, "duplicate local binding is legal");
        assert_eq!(&rows[2].0, "entry-a2");
        assert_eq!(rows[2].4, None);
        assert_eq!(rows[2].5, None);
        assert_eq!(rows[2].10.as_deref(), Some("/import/future.flac"));
        assert_eq!(&rows[3].0, "entry-b0");

        assert!(has_index(&db, PLAYLIST_INDEX).await);
        assert!(has_index(&db, SOURCE_TRACK_INDEX).await);
        assert!(has_index(&db, LOCAL_TRACK_INDEX).await);
        assert!(has_index(&db, UNIQUE_POSITION_INDEX).await);
        assert!(has_index(&db, "idx_playlist_entries_match_title_custom").await);
        assert!(!has_index(&db, LEGACY_TRACK_INDEX).await);
        assert!(!migration_table_exists(&db).await);
    }

    #[tokio::test]
    async fn scoped_constraints_use_byte_bounds_and_keep_remote_rows_locator_free() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "local").await;
        insert_track(&db, "other").await;
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade source-scoped entries");

        insert_scoped_entry(
            &db,
            "local-orphan",
            "playlist",
            0,
            LOCAL_SOURCE_ID,
            None,
            None,
            Some("/import/orphan.flac"),
        )
        .await
        .expect("local fingerprint-only orphan remains representable");
        insert_scoped_entry(
            &db,
            "remote-valid",
            "playlist",
            1,
            REMOTE_SOURCE_ID,
            Some("native-id"),
            None,
            None,
        )
        .await
        .expect("valid remote identity");
        insert_scoped_entry(
            &db,
            "local-stale",
            "playlist",
            2,
            LOCAL_SOURCE_ID,
            Some("remembered-id"),
            None,
            None,
        )
        .await
        .expect("durable local identity may outlive its binding");
        insert_scoped_entry(
            &db,
            "local-fingerprint-orphan",
            "playlist",
            3,
            LOCAL_SOURCE_ID,
            None,
            None,
            None,
        )
        .await
        .expect("nonblank title and artist retain a local orphan");

        assert!(db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO playlist_entries (
                     id, playlist_id, position, source_id, track_id, local_track_id,
                     match_title, match_artist, match_album, match_duration_secs,
                     match_file_path
                 )
                 VALUES ('empty-evidence', 'playlist', 10, ?, NULL, NULL,
                         '  ', char(9) || char(10), '', NULL, '   ')",
                [LOCAL_SOURCE_ID.into()],
            ))
            .await
            .is_err());

        for (index, code_point) in RUST_TRIM_WHITESPACE.iter().enumerate() {
            let whitespace = char::from_u32(*code_point)
                .expect("frozen whitespace code point is valid")
                .to_string();
            assert!(whitespace.trim().is_empty());

            for (suffix, title, artist, path) in [
                (
                    "path",
                    String::new(),
                    String::new(),
                    Some(whitespace.clone()),
                ),
                ("fingerprint", whitespace.clone(), whitespace.clone(), None),
            ] {
                let id = format!("unicode-whitespace-{index}-{suffix}");
                let result = db
                    .execute(Statement::from_sql_and_values(
                        DbBackend::Sqlite,
                        "INSERT INTO playlist_entries (
                             id, playlist_id, position, source_id, track_id, local_track_id,
                             match_title, match_artist, match_album, match_duration_secs,
                             match_file_path
                         )
                         VALUES (?, 'playlist', 10, ?, NULL, NULL, ?, ?, '', NULL, ?)",
                        [
                            id.clone().into(),
                            LOCAL_SOURCE_ID.into(),
                            title.into(),
                            artist.into(),
                            path.into(),
                        ],
                    ))
                    .await;
                assert!(
                    result.is_err(),
                    "Rust-trim whitespace U+{code_point:04X} supplied usable {suffix} evidence"
                );
            }
        }

        for (id, source_id, track_id, local_track_id, path) in [
            (
                "source-upper",
                "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
                Some("id"),
                None,
                None,
            ),
            ("source-nil", NIL_SOURCE_ID, Some("id"), None, None),
            (
                "source-short",
                "11111111-1111-4111-8111-11111111111",
                Some("id"),
                None,
                None,
            ),
            ("remote-null", REMOTE_SOURCE_ID, None, None, None),
            ("remote-empty", REMOTE_SOURCE_ID, Some(""), None, None),
            (
                "remote-binding",
                REMOTE_SOURCE_ID,
                Some("local"),
                Some("local"),
                None,
            ),
            (
                "remote-path",
                REMOTE_SOURCE_ID,
                Some("id"),
                None,
                Some("/secret/locator"),
            ),
            (
                "local-mismatch",
                LOCAL_SOURCE_ID,
                Some("local"),
                Some("other"),
                None,
            ),
            (
                "local-dangling",
                LOCAL_SOURCE_ID,
                Some("missing"),
                Some("missing"),
                None,
            ),
        ] {
            assert!(
                insert_scoped_entry(
                    &db,
                    id,
                    "playlist",
                    10,
                    source_id,
                    track_id,
                    local_track_id,
                    path,
                )
                .await
                .is_err(),
                "invalid scoped entry {id} was accepted"
            );
        }

        let remote_over_byte_limit = "é".repeat(2_049);
        assert_eq!(remote_over_byte_limit.chars().count(), 2_049);
        assert_eq!(remote_over_byte_limit.len(), 4_098);
        assert!(insert_scoped_entry(
            &db,
            "remote-over-bytes",
            "playlist",
            10,
            SECOND_REMOTE_SOURCE_ID,
            Some(&remote_over_byte_limit),
            None,
            None,
        )
        .await
        .is_err());

        let local_over_byte_limit = "é".repeat(131_073);
        assert_eq!(local_over_byte_limit.len(), 262_146);
        assert!(insert_scoped_entry(
            &db,
            "local-over-bytes",
            "playlist",
            10,
            LOCAL_SOURCE_ID,
            Some(&local_over_byte_limit),
            None,
            None,
        )
        .await
        .is_err());

        assert!(insert_scoped_entry(
            &db,
            "negative-position",
            "playlist",
            -1,
            LOCAL_SOURCE_ID,
            Some("local"),
            Some("local"),
            None,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn corrupt_predecessor_orphan_fails_atomically_and_can_be_repaired_and_retried() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_legacy_entry(&db, "empty-orphan", "playlist", 0, None, None).await;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE playlist_entries
             SET match_title = ?, match_artist = ?, match_file_path = ?
             WHERE id = 'empty-orphan'",
            [
                "\u{00a0}".into(),
                "\u{000c}".into(),
                "\u{00a0}\u{000c}".into(),
            ],
        ))
        .await
        .expect("create Unicode-whitespace-only predecessor orphan");

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("orphan without identity or match evidence must fail");
        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::LegacyLocal)
        );
        assert_eq!(
            row_count(&db, "id = 'empty-orphan' AND track_id IS NULL").await,
            1
        );
        assert!(!migration_table_exists(&db).await);

        db.execute_unprepared(
            "UPDATE playlist_entries
             SET match_title = 'Recovered', match_artist = 'Artist'
             WHERE id = 'empty-orphan'",
        )
        .await
        .expect("repair predecessor evidence");
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("retry accepts repaired fingerprint evidence");
    }

    #[tokio::test]
    async fn near_match_legacy_table_is_rejected_before_rebuild_without_changing_data() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "track").await;
        insert_legacy_entry(&db, "entry", "playlist", 0, Some("track"), None).await;
        install_near_match_legacy_schema(&db).await;
        let before_sql = table_sql(&SchemaManager::new(&db))
            .await
            .expect("near-match table SQL");
        assert!(before_sql.contains("unexpected_position_check"));

        let error = Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("near-match predecessor must not be adopted");
        assert!(error.to_string().contains("exact migration-12 predecessor"));
        assert_eq!(
            table_sql(&SchemaManager::new(&db)).await.unwrap(),
            before_sql
        );
        assert_eq!(
            row_count(&db, "id = 'entry' AND track_id = 'track'").await,
            1
        );
        assert!(has_index(&db, LEGACY_TRACK_INDEX).await);
        assert!(!migration_table_exists(&db).await);
    }

    #[tokio::test]
    async fn target_literal_mutations_are_rejected_without_changing_schema() {
        for mutation in ["uppercase-local-source", "missing-ascii-space-trim"] {
            let db = database_before_migration().await;
            insert_playlist(&db, "playlist").await;
            Migration
                .up(&SchemaManager::new(&db))
                .await
                .expect("install exact scoped schema");

            let exact_sql = scoped_table_sql("near_match_playlist_entries");
            let near_sql = match mutation {
                "uppercase-local-source" => {
                    exact_sql.replacen(LOCAL_SOURCE_ID, &LOCAL_SOURCE_ID.to_ascii_uppercase(), 1)
                }
                "missing-ascii-space-trim" => exact_sql.replacen(
                    "char(13) || ' ' || char(133)",
                    "char(13) || '' || char(133)",
                    1,
                ),
                _ => unreachable!("fixed mutation list"),
            };
            assert_ne!(near_sql, exact_sql, "test mutation must alter table SQL");
            install_near_match_scoped_schema(&db, &near_sql).await;
            let before_sql = table_sql(&SchemaManager::new(&db))
                .await
                .expect("near-match target SQL");

            let error = Migration
                .up(&SchemaManager::new(&db))
                .await
                .expect_err("literal-mutated target must not be adopted");
            assert!(error
                .to_string()
                .contains("source-scoped playlist_entries has unexpected table SQL"));
            assert_eq!(
                table_sql(&SchemaManager::new(&db)).await.unwrap(),
                before_sql
            );
            assert_eq!(row_count(&db, "1 = 1").await, 0);
            assert!(has_index(&db, SOURCE_TRACK_INDEX).await);
            assert!(!migration_table_exists(&db).await);
        }
    }

    #[tokio::test]
    async fn partial_standard_index_is_rejected_without_rebuilding_and_retry_is_clean() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "track").await;
        insert_legacy_entry(&db, "entry", "playlist", 0, Some("track"), None).await;
        db.execute_unprepared(&format!("DROP INDEX {LEGACY_TRACK_INDEX}"))
            .await
            .expect("drop exact track index");
        db.execute_unprepared(&format!(
            "CREATE INDEX {LEGACY_TRACK_INDEX}
             ON playlist_entries (track_id) WHERE track_id IS NOT NULL"
        ))
        .await
        .expect("install partial lookalike");

        let error = Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("partial standard index must be rejected");
        assert!(error.to_string().contains("partial=true"));
        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::LegacyLocal)
        );
        assert_eq!(row_count(&db, "id = 'entry'").await, 1);
        assert!(!migration_table_exists(&db).await);

        db.execute_unprepared(&format!("DROP INDEX {LEGACY_TRACK_INDEX}"))
            .await
            .expect("drop partial lookalike");
        db.execute_unprepared(&format!(
            "CREATE INDEX {LEGACY_TRACK_INDEX} ON playlist_entries (track_id)"
        ))
        .await
        .expect("restore exact track index");
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("retry after index repair");
    }

    #[tokio::test]
    async fn collation_lookalike_standard_index_is_rejected_and_retry_is_clean() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "track").await;
        insert_legacy_entry(&db, "entry", "playlist", 0, Some("track"), None).await;
        db.execute_unprepared(&format!("DROP INDEX {UNIQUE_POSITION_INDEX}"))
            .await
            .expect("drop exact position index");
        db.execute_unprepared(&format!(
            "CREATE UNIQUE INDEX {UNIQUE_POSITION_INDEX}
             ON playlist_entries (playlist_id COLLATE NOCASE, position)"
        ))
        .await
        .expect("install collation lookalike");

        let error = Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("collation lookalike must be rejected");
        assert!(error.to_string().contains("has unexpected SQL"));
        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::LegacyLocal)
        );
        assert_eq!(row_count(&db, "id = 'entry'").await, 1);
        assert!(!migration_table_exists(&db).await);

        db.execute_unprepared(&format!("DROP INDEX {UNIQUE_POSITION_INDEX}"))
            .await
            .expect("drop collation lookalike");
        db.execute_unprepared(&format!(
            "CREATE UNIQUE INDEX {UNIQUE_POSITION_INDEX}
             ON playlist_entries (playlist_id, position)"
        ))
        .await
        .expect("restore exact position index");
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("retry after index repair");
    }

    #[tokio::test]
    async fn local_deletion_clears_only_the_binding_and_never_collides_with_remote_identity() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "same-native-id").await;
        insert_legacy_entry(
            &db,
            "local-entry",
            "playlist",
            0,
            Some("same-native-id"),
            None,
        )
        .await;
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade source-scoped entries");
        insert_scoped_entry(
            &db,
            "remote-entry",
            "playlist",
            1,
            REMOTE_SOURCE_ID,
            Some("same-native-id"),
            None,
            None,
        )
        .await
        .expect("insert colliding remote-native ID");

        db.execute_unprepared("DELETE FROM tracks WHERE id = 'same-native-id'")
            .await
            .expect("delete local track");

        let rows = scoped_rows(&db).await;
        assert_eq!(rows[0].3, LOCAL_SOURCE_ID);
        assert_eq!(rows[0].4.as_deref(), Some("same-native-id"));
        assert_eq!(rows[0].5, None);
        assert_eq!(rows[1].3, REMOTE_SOURCE_ID);
        assert_eq!(rows[1].4.as_deref(), Some("same-native-id"));
        assert_eq!(rows[1].5, None);

        db.execute_unprepared("DELETE FROM playlists WHERE id = 'playlist'")
            .await
            .expect("delete playlist");
        assert_eq!(row_count(&db, "1 = 1").await, 0);
    }

    #[tokio::test]
    async fn downgrade_round_trips_only_losslessly_representable_local_rows() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "linked").await;
        insert_legacy_entry(&db, "linked-entry", "playlist", 0, Some("linked"), None).await;
        insert_legacy_entry(
            &db,
            "orphan-entry",
            "playlist",
            1,
            None,
            Some("/import/orphan.flac"),
        )
        .await;
        db.execute_unprepared(
            "CREATE INDEX idx_playlist_entries_match_artist_custom
             ON playlist_entries (match_artist)",
        )
        .await
        .expect("create custom index");

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade source-scoped entries");
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("lossless local downgrade");
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("repeat downgrade validates in place");

        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::LegacyLocal)
        );
        validate_schema(&SchemaManager::new(&db), PlaylistEntriesSchema::LegacyLocal)
            .await
            .expect("validate restored legacy schema");
        assert!(has_index(&db, LEGACY_TRACK_INDEX).await);
        assert!(has_index(&db, "idx_playlist_entries_match_artist_custom").await);
        let rows = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, position, track_id, match_file_path
                 FROM playlist_entries ORDER BY position"
                    .to_string(),
            ))
            .await
            .expect("query downgraded rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].try_get::<String>("", "id").unwrap(), "linked-entry");
        assert_eq!(
            rows[0]
                .try_get::<Option<String>>("", "track_id")
                .unwrap()
                .as_deref(),
            Some("linked")
        );
        assert_eq!(
            rows[1].try_get::<Option<String>>("", "track_id").unwrap(),
            None
        );
        assert_eq!(
            rows[1]
                .try_get::<Option<String>>("", "match_file_path")
                .unwrap()
                .as_deref(),
            Some("/import/orphan.flac")
        );

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade succeeds again after downgrade");
    }

    #[tokio::test]
    async fn downgrade_refuses_remote_and_deleted_local_identity_without_changing_data() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "local").await;
        insert_legacy_entry(&db, "local-entry", "playlist", 0, Some("local"), None).await;
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade source-scoped entries");
        insert_scoped_entry(
            &db,
            "remote-entry",
            "playlist",
            1,
            REMOTE_SOURCE_ID,
            Some("remote-track"),
            None,
            None,
        )
        .await
        .expect("insert remote entry");
        let before = scoped_rows(&db).await;

        let error = Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("remote row cannot be downgraded losslessly");
        assert!(error.to_string().contains("cannot downgrade 1"));
        assert_eq!(scoped_rows(&db).await, before);
        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::SourceScoped)
        );

        db.execute_unprepared("DELETE FROM playlist_entries WHERE id = 'remote-entry'")
            .await
            .expect("remove remote test row");
        db.execute_unprepared("DELETE FROM tracks WHERE id = 'local'")
            .await
            .expect("delete local binding");
        let deleted_local = scoped_rows(&db).await;
        assert_eq!(deleted_local[0].4.as_deref(), Some("local"));
        assert_eq!(deleted_local[0].5, None);
        let error = Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("deleted-local durable identity cannot be downgraded losslessly");
        assert!(error.to_string().contains("cannot downgrade 1"));
        assert_eq!(scoped_rows(&db).await, deleted_local);

        db.execute_unprepared(
            "UPDATE playlist_entries SET track_id = NULL WHERE id = 'local-entry'",
        )
        .await
        .expect("explicitly discard unrepresentable durable identity");
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("downgrade becomes lossless and retryable");
    }

    #[tokio::test]
    async fn mid_rebuild_index_failure_rolls_back_exactly_and_retry_succeeds() {
        let db = database_before_migration().await;
        insert_playlist(&db, "playlist").await;
        insert_track(&db, "track").await;
        insert_legacy_entry(&db, "entry", "playlist", 0, Some("track"), None).await;
        db.execute_unprepared("CREATE TABLE unrelated (id VARCHAR NOT NULL)")
            .await
            .expect("create unrelated table");
        db.execute_unprepared(&format!(
            "CREATE INDEX {SOURCE_TRACK_INDEX} ON unrelated (id)"
        ))
        .await
        .expect("reserve target index name");

        let error = Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("target index collision must fail after rebuild begins");
        assert!(error.to_string().contains(SOURCE_TRACK_INDEX));
        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::LegacyLocal)
        );
        assert_eq!(
            row_count(&db, "id = 'entry' AND track_id = 'track'").await,
            1
        );
        assert!(has_index(&db, LEGACY_TRACK_INDEX).await);
        assert!(!migration_table_exists(&db).await);

        db.execute_unprepared(&format!("DROP INDEX {SOURCE_TRACK_INDEX}"))
            .await
            .expect("remove injected collision");
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("retry upgrade after rollback");
        assert_eq!(
            inspect_schema(&SchemaManager::new(&db)).await,
            Ok(PlaylistEntriesSchema::SourceScoped)
        );
        assert_eq!(
            row_count(&db, "id = 'entry' AND track_id = 'track'").await,
            1
        );
    }
}
