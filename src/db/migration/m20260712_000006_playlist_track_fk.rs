//! Migration: enforce playlist-entry references to local tracks.
//!
//! The original playlist table declared `track_id` as a nullable string but
//! omitted its database foreign key. Deleting a track could therefore leave a
//! non-null dangling ID that reconciliation would never inspect. SQLite cannot
//! add a foreign key in place, so this migration transactionally rebuilds the
//! table, nulls existing dangling IDs, and restores every index.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

const REBUILD_TABLE: &str = "tributary_playlist_entries_track_fk_rebuild";
const PLAYLIST_INDEX: &str = "idx_playlist_entries_playlist_id";
const TRACK_INDEX: &str = "idx_playlist_entries_track_id";
const UNIQUE_POSITION_INDEX: &str = "idx_playlist_entries_playlist_position_unique";

#[derive(Debug, Eq, PartialEq)]
enum PlaylistEntriesSchema {
    Legacy,
    WithTrackForeignKey,
}

#[derive(Debug)]
struct ExplicitIndex {
    name: String,
    sql: String,
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild_playlist_entries(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rebuild_playlist_entries(manager, false).await
    }
}

/// Rebuild `playlist_entries` inside one explicit transaction.
///
/// SeaORM does not automatically wrap SQLite migrations in a transaction.
/// Owning it here ensures a failed copy, constraint check, or index recreation
/// restores the original table exactly and leaves the migration retryable.
async fn rebuild_playlist_entries(
    manager: &SchemaManager<'_>,
    include_track_foreign_key: bool,
) -> Result<(), DbErr> {
    if !manager.has_table("playlist_entries").await? {
        return Err(DbErr::Migration(
            "playlist_entries must exist for the track foreign-key migration".to_string(),
        ));
    }

    let transaction = manager.get_connection().begin().await?;
    let result = {
        let manager = SchemaManager::new(&transaction);
        let schema = inspect_playlist_entries_schema(&manager).await?;
        let indexes = capture_and_validate_indexes(&manager).await?;
        let already_has_requested_schema = matches!(
            (include_track_foreign_key, schema),
            (true, PlaylistEntriesSchema::WithTrackForeignKey)
                | (false, PlaylistEntriesSchema::Legacy)
        );
        if already_has_requested_schema {
            if include_track_foreign_key {
                null_dangling_track_ids(&manager).await?;
            }
            validate_playlist_entry_foreign_keys(&manager).await
        } else {
            rebuild_playlist_entries_in_transaction(&manager, include_track_foreign_key, &indexes)
                .await
        }
    };

    match result {
        Ok(()) => transaction.commit().await,
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

async fn rebuild_playlist_entries_in_transaction(
    manager: &SchemaManager<'_>,
    include_track_foreign_key: bool,
    indexes: &[ExplicitIndex],
) -> Result<(), DbErr> {
    let connection = manager.get_connection();
    let track_foreign_key = if include_track_foreign_key {
        ",
         CONSTRAINT fk_entry_track
             FOREIGN KEY (track_id)
             REFERENCES tracks (id)
             ON DELETE SET NULL"
    } else {
        ""
    };

    connection
        .execute_unprepared(&format!(
            "CREATE TABLE {REBUILD_TABLE} (
                 id VARCHAR PRIMARY KEY NOT NULL,
                 playlist_id VARCHAR NOT NULL,
                 position INTEGER NOT NULL,
                 track_id VARCHAR NULL,
                 match_title VARCHAR NOT NULL DEFAULT '',
                 match_artist VARCHAR NOT NULL DEFAULT '',
                 match_album VARCHAR NOT NULL DEFAULT '',
                 match_duration_secs INTEGER NULL,
                 CONSTRAINT fk_entry_playlist
                     FOREIGN KEY (playlist_id)
                     REFERENCES playlists (id)
                     ON DELETE CASCADE
                 {track_foreign_key}
             )"
        ))
        .await?;

    if include_track_foreign_key {
        null_dangling_track_ids(manager).await?;
    }

    connection
        .execute_unprepared(&format!(
            "INSERT INTO {REBUILD_TABLE} (
                 id,
                 playlist_id,
                 position,
                 track_id,
                 match_title,
                 match_artist,
                 match_album,
                 match_duration_secs
             )
             SELECT source.id,
                    source.playlist_id,
                    source.position,
                    source.track_id,
                    source.match_title,
                    source.match_artist,
                    source.match_album,
                    source.match_duration_secs
             FROM playlist_entries AS source"
        ))
        .await?;

    connection
        .execute_unprepared("DROP TABLE playlist_entries")
        .await?;
    connection
        .execute_unprepared(&format!(
            "ALTER TABLE {REBUILD_TABLE} RENAME TO playlist_entries"
        ))
        .await?;

    for index in indexes {
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

    validate_playlist_entry_foreign_keys(manager).await
}

async fn null_dangling_track_ids(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .get_connection()
        .execute_unprepared(
            "UPDATE playlist_entries
             SET track_id = NULL
             WHERE track_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1
                   FROM tracks
                   WHERE tracks.id = playlist_entries.track_id
               )",
        )
        .await?;
    Ok(())
}

async fn validate_playlist_entry_foreign_keys(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let violations = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA foreign_key_check('playlist_entries')".to_string(),
        ))
        .await?;
    if !violations.is_empty() {
        return Err(DbErr::Migration(format!(
            "rebuilt playlist_entries has {} foreign-key violation(s)",
            violations.len()
        )));
    }

    Ok(())
}

async fn inspect_playlist_entries_schema(
    manager: &SchemaManager<'_>,
) -> Result<PlaylistEntriesSchema, DbErr> {
    let connection = manager.get_connection();
    let backend = manager.get_database_backend();
    let columns = connection
        .query_all(Statement::from_string(
            backend,
            "PRAGMA table_info('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<String>("", "name")?,
                row.try_get::<String>("", "type")?.to_ascii_lowercase(),
                row.try_get::<i32>("", "notnull")?,
                row.try_get::<Option<String>>("", "dflt_value")?,
                row.try_get::<i32>("", "pk")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected_columns = vec![
        ("id".to_string(), "varchar".to_string(), 1, None, 1),
        ("playlist_id".to_string(), "varchar".to_string(), 1, None, 0),
        ("position".to_string(), "integer".to_string(), 1, None, 0),
        ("track_id".to_string(), "varchar".to_string(), 0, None, 0),
        (
            "match_title".to_string(),
            "varchar".to_string(),
            1,
            Some("''".to_string()),
            0,
        ),
        (
            "match_artist".to_string(),
            "varchar".to_string(),
            1,
            Some("''".to_string()),
            0,
        ),
        (
            "match_album".to_string(),
            "varchar".to_string(),
            1,
            Some("''".to_string()),
            0,
        ),
        (
            "match_duration_secs".to_string(),
            "integer".to_string(),
            0,
            None,
            0,
        ),
    ];
    if columns != expected_columns {
        return Err(DbErr::Migration(format!(
            "playlist_entries has an unexpected column schema: {columns:?}"
        )));
    }

    let foreign_keys = connection
        .query_all(Statement::from_string(
            backend,
            "PRAGMA foreign_key_list('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<String>("", "from")?,
                row.try_get::<String>("", "table")?,
                row.try_get::<String>("", "to")?,
                row.try_get::<String>("", "on_update")?,
                row.try_get::<String>("", "on_delete")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let playlist_foreign_key = (
        "playlist_id".to_string(),
        "playlists".to_string(),
        "id".to_string(),
        "NO ACTION".to_string(),
        "CASCADE".to_string(),
    );
    let track_foreign_key = (
        "track_id".to_string(),
        "tracks".to_string(),
        "id".to_string(),
        "NO ACTION".to_string(),
        "SET NULL".to_string(),
    );

    if foreign_keys.len() == 1 && foreign_keys.contains(&playlist_foreign_key) {
        Ok(PlaylistEntriesSchema::Legacy)
    } else if foreign_keys.len() == 2
        && foreign_keys.contains(&playlist_foreign_key)
        && foreign_keys.contains(&track_foreign_key)
    {
        Ok(PlaylistEntriesSchema::WithTrackForeignKey)
    } else {
        Err(DbErr::Migration(format!(
            "playlist_entries has unexpected foreign keys: {foreign_keys:?}"
        )))
    }
}

async fn capture_and_validate_indexes(
    manager: &SchemaManager<'_>,
) -> Result<Vec<ExplicitIndex>, DbErr> {
    let connection = manager.get_connection();
    let backend = manager.get_database_backend();
    let indexes = connection
        .query_all(Statement::from_string(
            backend,
            "SELECT name, sql
             FROM sqlite_master
             WHERE type = 'index'
               AND tbl_name = 'playlist_entries'
               AND sql IS NOT NULL
             ORDER BY name"
                .to_string(),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>(ExplicitIndex {
                name: row.try_get("", "name")?,
                sql: row.try_get("", "sql")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    validate_implicit_primary_key_index(connection, backend).await?;

    validate_index(
        connection,
        backend,
        &indexes,
        PLAYLIST_INDEX,
        false,
        &["playlist_id"],
    )
    .await?;
    validate_index(
        connection,
        backend,
        &indexes,
        TRACK_INDEX,
        false,
        &["track_id"],
    )
    .await?;
    validate_index(
        connection,
        backend,
        &indexes,
        UNIQUE_POSITION_INDEX,
        true,
        &["playlist_id", "position"],
    )
    .await?;

    Ok(indexes)
}

async fn validate_implicit_primary_key_index<C>(
    connection: &C,
    backend: DbBackend,
) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    let implicit_indexes = connection
        .query_all(Statement::from_string(
            backend,
            "PRAGMA index_list('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<String>("", "name")?,
                row.try_get::<i32>("", "unique")? == 1,
                row.try_get::<String>("", "origin")?,
                row.try_get::<i32>("", "partial").unwrap_or_default() == 1,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(_, _, origin, _)| origin != "c")
        .collect::<Vec<_>>();

    let [(name, true, origin, false)] = implicit_indexes.as_slice() else {
        return Err(DbErr::Migration(format!(
            "playlist_entries has unexpected implicit indexes: {implicit_indexes:?}"
        )));
    };
    if origin != "pk" {
        return Err(DbErr::Migration(format!(
            "playlist_entries has unexpected implicit index origin {origin}"
        )));
    }

    let columns = connection
        .query_all(Statement::from_string(
            backend,
            format!("PRAGMA index_info('{name}')"),
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
    if columns != vec![(0, Some("id".to_string()))] {
        return Err(DbErr::Migration(format!(
            "playlist_entries primary-key index has unexpected columns: {columns:?}"
        )));
    }

    Ok(())
}

async fn validate_index<C>(
    connection: &C,
    backend: DbBackend,
    captured: &[ExplicitIndex],
    name: &str,
    expected_unique: bool,
    expected_columns: &[&str],
) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    if !captured.iter().any(|index| index.name == name) {
        return Err(DbErr::Migration(format!(
            "playlist_entries is missing required index {name}"
        )));
    }

    let index_row = connection
        .query_all(Statement::from_string(
            backend,
            "PRAGMA index_list('playlist_entries')".to_string(),
        ))
        .await?
        .into_iter()
        .find(|row| {
            row.try_get::<String>("", "name")
                .is_ok_and(|value| value == name)
        })
        .ok_or_else(|| DbErr::Migration(format!("SQLite did not expose index {name}")))?;
    let unique = index_row.try_get::<i32>("", "unique")? == 1;
    let partial = index_row.try_get::<i32>("", "partial").unwrap_or_default() == 1;
    let mut columns = connection
        .query_all(Statement::from_string(
            backend,
            format!("PRAGMA index_info('{name}')"),
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
    let columns = columns
        .into_iter()
        .map(|(_, column)| column)
        .collect::<Vec<_>>();
    let expected_columns = expected_columns
        .iter()
        .map(|column| Some((*column).to_string()))
        .collect::<Vec<_>>();

    if unique != expected_unique || partial || columns != expected_columns {
        return Err(DbErr::Migration(format!(
            "required index {name} has unexpected shape: unique={unique}, partial={partial}, columns={columns:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_before_track_fk_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(5))
            .await
            .expect("apply migrations preceding playlist track foreign key");
        db
    }

    async fn insert_playlist(db: &DatabaseConnection, id: &str) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES (?, ?, '2026-07-12T00:00:00Z', '2026-07-12T00:00:00Z')",
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
                     '2026-07-12T00:00:00Z', '2026-07-12T00:00:00Z')",
            [
                id.into(),
                format!("/music/{id}.flac").into(),
                format!("Track {id}").into(),
            ],
        ))
        .await
        .expect("insert track");
    }

    async fn insert_entry(
        db: &DatabaseConnection,
        id: &str,
        playlist_id: &str,
        position: i32,
        track_id: Option<&str>,
    ) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, track_id,
                 match_title, match_artist, match_album, match_duration_secs
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            [
                id.into(),
                playlist_id.into(),
                position.into(),
                track_id.into(),
                format!("title-{id}").into(),
                format!("artist-{id}").into(),
                format!("album-{id}").into(),
                (position + 180).into(),
            ],
        ))
        .await
        .expect("insert playlist entry");
    }

    async fn apply_track_fk_migration(db: &DatabaseConnection) -> Result<(), DbErr> {
        Migrator::up(db, Some(1)).await
    }

    async fn track_fk_migration_is_applied(db: &DatabaseConnection) -> bool {
        let migration_name = Migration.name().to_string();
        Migrator::get_migration_models(db)
            .await
            .expect("query migration ledger")
            .iter()
            .any(|migration| migration.version == migration_name)
    }

    async fn entry_rows(
        db: &DatabaseConnection,
    ) -> Vec<(
        String,
        i32,
        Option<String>,
        String,
        String,
        String,
        Option<i32>,
    )> {
        db.query_all(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT id, position, track_id, match_title, match_artist,
                    match_album, match_duration_secs
             FROM playlist_entries
             ORDER BY position"
                .to_string(),
        ))
        .await
        .expect("query playlist entries")
        .into_iter()
        .map(|row| {
            (
                row.try_get("", "id").expect("entry id"),
                row.try_get("", "position").expect("entry position"),
                row.try_get("", "track_id").expect("entry track id"),
                row.try_get("", "match_title").expect("match title"),
                row.try_get("", "match_artist").expect("match artist"),
                row.try_get("", "match_album").expect("match album"),
                row.try_get("", "match_duration_secs")
                    .expect("match duration"),
            )
        })
        .collect()
    }

    async fn track_foreign_key(db: &DatabaseConnection) -> Option<(String, String, String)> {
        db.query_all(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_key_list('playlist_entries')".to_string(),
        ))
        .await
        .expect("query playlist entry foreign keys")
        .into_iter()
        .find_map(|row| {
            let from: String = row.try_get("", "from").ok()?;
            (from == "track_id").then(|| {
                (
                    row.try_get("", "table").expect("foreign table"),
                    row.try_get("", "to").expect("foreign column"),
                    row.try_get("", "on_delete").expect("delete action"),
                )
            })
        })
    }

    async fn playlist_foreign_key(db: &DatabaseConnection) -> Option<(String, String, String)> {
        db.query_all(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_key_list('playlist_entries')".to_string(),
        ))
        .await
        .expect("query playlist entry foreign keys")
        .into_iter()
        .find_map(|row| {
            let from: String = row.try_get("", "from").ok()?;
            (from == "playlist_id").then(|| {
                (
                    row.try_get("", "table").expect("foreign table"),
                    row.try_get("", "to").expect("foreign column"),
                    row.try_get("", "on_delete").expect("delete action"),
                )
            })
        })
    }

    async fn index_definition(db: &DatabaseConnection, name: &str) -> Option<(bool, Vec<String>)> {
        let index_row = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA index_list('playlist_entries')".to_string(),
            ))
            .await
            .expect("query playlist entry indexes")
            .into_iter()
            .find(|row| {
                row.try_get::<String>("", "name")
                    .is_ok_and(|value| value == name)
            })?;
        let unique = index_row
            .try_get::<i32>("", "unique")
            .expect("index uniqueness")
            == 1;
        let mut columns = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                format!("PRAGMA index_info('{name}')"),
            ))
            .await
            .expect("query index columns")
            .into_iter()
            .map(|row| {
                (
                    row.try_get::<i32>("", "seqno").expect("column sequence"),
                    row.try_get::<String>("", "name").expect("column name"),
                )
            })
            .collect::<Vec<_>>();
        columns.sort_by_key(|(sequence, _)| *sequence);
        Some((
            unique,
            columns.into_iter().map(|(_, column)| column).collect(),
        ))
    }

    async fn index_sql(db: &DatabaseConnection, name: &str) -> Option<String> {
        db.query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?",
            [name.into()],
        ))
        .await
        .expect("query index SQL")
        .map(|row| row.try_get("", "sql").expect("index SQL"))
    }

    #[tokio::test]
    async fn upgrade_preserves_entries_and_nulls_only_dangling_track_ids() {
        let db = database_before_track_fk_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_track(&db, "track-valid").await;
        insert_entry(&db, "entry-valid", "playlist-a", 0, Some("track-valid")).await;
        insert_entry(
            &db,
            "entry-dangling",
            "playlist-a",
            1,
            Some("track-missing"),
        )
        .await;
        insert_entry(&db, "entry-null", "playlist-a", 2, None).await;
        let before = entry_rows(&db).await;

        apply_track_fk_migration(&db)
            .await
            .expect("apply playlist track foreign key migration");

        let after = entry_rows(&db).await;
        assert_eq!(after.len(), before.len());
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].0, before[1].0);
        assert_eq!(after[1].1, before[1].1);
        assert_eq!(after[1].2, None);
        assert_eq!(after[1].3, before[1].3);
        assert_eq!(after[1].4, before[1].4);
        assert_eq!(after[1].5, before[1].5);
        assert_eq!(after[1].6, before[1].6);
        assert_eq!(after[2], before[2]);
    }

    #[tokio::test]
    async fn rebuilt_schema_enforces_both_foreign_keys_and_restores_indexes() {
        let db = database_before_track_fk_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_track(&db, "track-a").await;
        insert_entry(&db, "entry-a", "playlist-a", 0, Some("track-a")).await;
        db.execute_unprepared(
            "CREATE INDEX idx_playlist_entries_match_title_custom
             ON playlist_entries (match_title)
             WHERE match_title <> ''",
        )
        .await
        .expect("create custom playlist-entry index");

        apply_track_fk_migration(&db)
            .await
            .expect("apply playlist track foreign key migration");

        assert_eq!(
            playlist_foreign_key(&db).await,
            Some((
                "playlists".to_string(),
                "id".to_string(),
                "CASCADE".to_string(),
            ))
        );
        assert_eq!(
            track_foreign_key(&db).await,
            Some((
                "tracks".to_string(),
                "id".to_string(),
                "SET NULL".to_string(),
            ))
        );
        assert_eq!(
            index_definition(&db, PLAYLIST_INDEX).await,
            Some((false, vec!["playlist_id".to_string()]))
        );
        assert_eq!(
            index_definition(&db, TRACK_INDEX).await,
            Some((false, vec!["track_id".to_string()]))
        );
        assert_eq!(
            index_definition(&db, UNIQUE_POSITION_INDEX).await,
            Some((
                true,
                vec!["playlist_id".to_string(), "position".to_string()],
            ))
        );
        assert_eq!(
            index_definition(&db, "idx_playlist_entries_match_title_custom").await,
            Some((false, vec!["match_title".to_string()]))
        );
        assert_eq!(
            index_sql(&db, "idx_playlist_entries_match_title_custom").await,
            Some(
                "CREATE INDEX idx_playlist_entries_match_title_custom\n             ON playlist_entries (match_title)\n             WHERE match_title <> ''"
                    .to_string()
            )
        );

        let missing_track_insert = db
            .execute_unprepared(
                "INSERT INTO playlist_entries (id, playlist_id, position, track_id)
                 VALUES ('entry-missing', 'playlist-a', 1, 'track-missing')",
            )
            .await;
        assert!(
            missing_track_insert.is_err(),
            "a non-null missing track ID must violate the new foreign key"
        );
        let duplicate_position_insert = db
            .execute_unprepared(
                "INSERT INTO playlist_entries (id, playlist_id, position, track_id)
                 VALUES ('entry-duplicate-position', 'playlist-a', 0, NULL)",
            )
            .await;
        assert!(
            duplicate_position_insert.is_err(),
            "the restored unique position index must reject duplicates"
        );

        db.execute_unprepared("DELETE FROM tracks WHERE id = 'track-a'")
            .await
            .expect("delete referenced track");
        let track_id: Option<String> = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT track_id FROM playlist_entries WHERE id = 'entry-a'".to_string(),
            ))
            .await
            .expect("query entry after track deletion")
            .expect("entry remains")
            .try_get("", "track_id")
            .expect("track id is nullable");
        assert_eq!(track_id, None);

        db.execute_unprepared("DELETE FROM playlists WHERE id = 'playlist-a'")
            .await
            .expect("delete referenced playlist");
        let remaining: i64 = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM playlist_entries".to_string(),
            ))
            .await
            .expect("count playlist entries")
            .expect("count row")
            .try_get("", "count")
            .expect("entry count");
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn failed_copy_rolls_back_dangling_id_cleanup_and_migration_can_retry() {
        let db = database_before_track_fk_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_track(&db, "track-a").await;
        insert_entry(&db, "entry-a", "playlist-a", 0, Some("track-a")).await;

        // Force a legacy-only playlist violation so the table copy fails after
        // the migration has already nulled dangling track IDs.
        db.execute_unprepared("PRAGMA foreign_keys = OFF")
            .await
            .expect("disable foreign keys for fault injection");
        insert_entry(
            &db,
            "entry-invalid-playlist",
            "playlist-missing",
            1,
            Some("track-missing"),
        )
        .await;
        db.execute_unprepared("PRAGMA foreign_keys = ON")
            .await
            .expect("restore foreign-key enforcement");
        let before = entry_rows(&db).await;

        assert!(
            apply_track_fk_migration(&db).await.is_err(),
            "invalid playlist reference must fail the rebuilt-table copy"
        );
        assert_eq!(entry_rows(&db).await, before);
        assert_eq!(track_foreign_key(&db).await, None);
        assert!(
            !SchemaManager::new(&db)
                .has_table(REBUILD_TABLE)
                .await
                .expect("inspect rebuild table"),
            "transaction rollback must remove the temporary rebuild table"
        );

        db.execute_unprepared("DELETE FROM playlist_entries WHERE id = 'entry-invalid-playlist'")
            .await
            .expect("remove injected invalid entry");
        apply_track_fk_migration(&db)
            .await
            .expect("retry playlist track foreign key migration");
        assert_eq!(
            track_foreign_key(&db).await,
            Some((
                "tracks".to_string(),
                "id".to_string(),
                "SET NULL".to_string(),
            ))
        );
        assert_eq!(entry_rows(&db).await, before[..1]);
    }

    #[tokio::test]
    async fn migration_can_be_reapplied_after_schema_commit_before_ledger_update() {
        let db = database_before_track_fk_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_track(&db, "track-a").await;
        insert_entry(&db, "entry-a", "playlist-a", 0, Some("track-a")).await;
        let before = entry_rows(&db).await;

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("apply schema change directly");
        db.execute_unprepared("PRAGMA foreign_keys = OFF")
            .await
            .expect("disable foreign keys for retry fixture");
        insert_entry(
            &db,
            "entry-dangling-after-schema-commit",
            "playlist-a",
            1,
            Some("track-missing"),
        )
        .await;
        db.execute_unprepared("PRAGMA foreign_keys = ON")
            .await
            .expect("restore foreign-key enforcement");
        apply_track_fk_migration(&db)
            .await
            .expect("retry already-committed schema change");
        assert!(track_fk_migration_is_applied(&db).await);

        let after = entry_rows(&db).await;
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].2, None);
        assert_eq!(
            track_foreign_key(&db).await,
            Some((
                "tracks".to_string(),
                "id".to_string(),
                "SET NULL".to_string(),
            ))
        );
    }

    #[tokio::test]
    async fn down_removes_only_the_track_foreign_key_and_is_repeatable() {
        let db = database_before_track_fk_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_track(&db, "track-a").await;
        insert_entry(&db, "entry-a", "playlist-a", 0, Some("track-a")).await;
        let before = entry_rows(&db).await;
        apply_track_fk_migration(&db)
            .await
            .expect("apply playlist track foreign key migration");

        // Simulate the schema rebuild committing before SeaORM removes the
        // migration-ledger row, then finish through the normal migrator.
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("apply down schema change directly");
        assert_eq!(track_foreign_key(&db).await, None);
        assert_eq!(
            playlist_foreign_key(&db).await,
            Some((
                "playlists".to_string(),
                "id".to_string(),
                "CASCADE".to_string(),
            ))
        );
        assert_eq!(entry_rows(&db).await, before);
        assert_eq!(
            index_definition(&db, UNIQUE_POSITION_INDEX).await,
            Some((
                true,
                vec!["playlist_id".to_string(), "position".to_string()],
            ))
        );

        Migrator::down(&db, Some(1))
            .await
            .expect("finish partially applied down migration");
        assert!(!track_fk_migration_is_applied(&db).await);
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("repeat direct down migration");
        assert_eq!(track_foreign_key(&db).await, None);
    }

    #[tokio::test]
    async fn down_rejects_a_missing_playlist_entries_table() {
        let db = database_before_track_fk_migration().await;
        apply_track_fk_migration(&db)
            .await
            .expect("apply playlist track foreign key migration");
        db.execute_unprepared("DROP TABLE playlist_entries")
            .await
            .expect("simulate catastrophic table loss");

        assert!(
            Migration.down(&SchemaManager::new(&db)).await.is_err(),
            "down must not treat a missing target table as its valid legacy state"
        );
    }

    #[tokio::test]
    async fn wrong_required_index_shape_fails_without_mutation_and_can_be_retried() {
        let db = database_before_track_fk_migration().await;
        insert_playlist(&db, "playlist-a").await;
        insert_track(&db, "track-a").await;
        insert_entry(&db, "entry-a", "playlist-a", 0, Some("track-a")).await;
        let before = entry_rows(&db).await;

        db.execute_unprepared(&format!("DROP INDEX {TRACK_INDEX}"))
            .await
            .expect("drop required track index");
        db.execute_unprepared(&format!(
            "CREATE INDEX {TRACK_INDEX} ON playlist_entries (match_title)"
        ))
        .await
        .expect("replace track index with wrong shape");

        assert!(
            apply_track_fk_migration(&db).await.is_err(),
            "preflight must reject a malformed required index"
        );
        assert_eq!(entry_rows(&db).await, before);
        assert_eq!(track_foreign_key(&db).await, None);
        assert!(!track_fk_migration_is_applied(&db).await);

        db.execute_unprepared(&format!("DROP INDEX {TRACK_INDEX}"))
            .await
            .expect("drop malformed track index");
        db.execute_unprepared(&format!(
            "CREATE INDEX {TRACK_INDEX} ON playlist_entries (track_id)"
        ))
        .await
        .expect("restore required track index");
        apply_track_fk_migration(&db)
            .await
            .expect("retry after restoring index shape");
        assert_eq!(entry_rows(&db).await, before);
        assert!(track_fk_migration_is_applied(&db).await);
    }
}
