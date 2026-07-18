//! Migration: add persisted playback-history metadata to local tracks.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::Statement;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("tracks", "last_played_at_ms").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(Tracks::Table)
                        .add_column(ColumnDef::new(Tracks::LastPlayedAtMs).big_integer().null())
                        .to_owned(),
                )
                .await?;
        }

        // SQLite can commit the ALTER before SeaORM records the migration.
        // Validate an already-present column before accepting it on retry.
        validate_last_played_column(manager).await?;

        // Older code could persist a signed count even though the public model
        // exposes an unsigned value. Repair only the invalid part of that
        // legacy range; all existing nonnegative counts remain untouched.
        manager
            .get_connection()
            .execute(Statement::from_string(
                manager.get_database_backend(),
                "UPDATE tracks SET play_count = 0 WHERE play_count < 0".to_string(),
            ))
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("tracks", "last_played_at_ms").await? {
            return Ok(());
        }

        manager
            .alter_table(
                Table::alter()
                    .table(Tracks::Table)
                    .drop_column(Tracks::LastPlayedAtMs)
                    .to_owned(),
            )
            .await
    }
}

/// Reject a same-named legacy or partially-created column with an incompatible
/// shape instead of silently treating it as the playback-history field.
async fn validate_last_played_column(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA table_info('tracks')".to_string(),
        ))
        .await?;

    let Some(row) = rows.into_iter().find(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|name| name == "last_played_at_ms")
    }) else {
        return Err(DbErr::Migration(
            "tracks.last_played_at_ms was not created".to_string(),
        ));
    };

    let column_type: String = row.try_get("", "type")?;
    let not_null: i32 = row.try_get("", "notnull")?;
    let default: Option<String> = row.try_get("", "dflt_value")?;
    let primary_key: i32 = row.try_get("", "pk")?;
    if !column_type.eq_ignore_ascii_case("bigint")
        || not_null != 0
        || default.is_some()
        || primary_key != 0
    {
        return Err(DbErr::Migration(format!(
            "tracks.last_played_at_ms has an unexpected schema: \
             type={column_type:?}, not_null={not_null}, default={default:?}, \
             primary_key={primary_key}"
        )));
    }

    Ok(())
}

#[derive(DeriveIden)]
enum Tracks {
    Table,
    LastPlayedAtMs,
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_before_playback_history_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(9))
            .await
            .expect("apply migrations preceding playback history");
        db
    }

    async fn insert_track(db: &DatabaseConnection, id: &str, play_count: i32) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO tracks (
                 id, file_path, title, artist_name, album_title, play_count,
                 date_added, date_modified
             ) VALUES (?, ?, 'Title', 'Artist', 'Album', ?, 'added', 'modified')",
            [
                id.into(),
                format!("/music/{id}.flac").into(),
                play_count.into(),
            ],
        ))
        .await
        .expect("insert pre-migration track");
    }

    async fn playback_column(db: &DatabaseConnection) -> Option<(String, i32, Option<String>)> {
        db.query_all(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA table_info('tracks')".to_string(),
        ))
        .await
        .expect("inspect track columns")
        .into_iter()
        .find_map(|row| {
            let name: String = row.try_get("", "name").ok()?;
            (name == "last_played_at_ms").then(|| {
                (
                    row.try_get("", "type").expect("playback timestamp type"),
                    row.try_get("", "notnull")
                        .expect("playback timestamp nullability"),
                    row.try_get("", "dflt_value")
                        .expect("playback timestamp default"),
                )
            })
        })
    }

    async fn track_history(db: &DatabaseConnection, id: &str) -> (i32, Option<i64>) {
        let row = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT play_count, last_played_at_ms FROM tracks WHERE id = ?",
                [id.into()],
            ))
            .await
            .expect("query migrated track")
            .expect("migrated track exists");
        (
            row.try_get("", "play_count").expect("play count"),
            row.try_get("", "last_played_at_ms")
                .expect("last-played timestamp"),
        )
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
    async fn up_adds_nullable_bigint_and_repairs_only_negative_counts() {
        let db = database_before_playback_history_migration().await;
        insert_track(&db, "positive", 27).await;
        insert_track(&db, "zero", 0).await;
        insert_track(&db, "negative", -4).await;

        Migrator::up(&db, Some(1))
            .await
            .expect("apply playback-history migration");

        assert_eq!(
            playback_column(&db).await,
            Some(("bigint".to_string(), 0, None))
        );
        assert_eq!(track_history(&db, "positive").await, (27, None));
        assert_eq!(track_history(&db, "zero").await, (0, None));
        assert_eq!(track_history(&db, "negative").await, (0, None));
    }

    #[tokio::test]
    async fn up_rejects_an_incompatible_existing_column_without_repairing_counts() {
        let db = database_before_playback_history_migration().await;
        insert_track(&db, "negative", -4).await;
        db.execute_unprepared(
            "ALTER TABLE tracks ADD COLUMN last_played_at_ms TEXT NOT NULL DEFAULT 'unknown'",
        )
        .await
        .expect("install incompatible legacy column");

        let error = Migrator::up(&db, Some(1))
            .await
            .expect_err("incompatible playback-history column must fail closed");
        assert!(
            error.to_string().contains("unexpected schema"),
            "unexpected migration error: {error}"
        );
        assert!(!migration_is_applied(&db).await);

        let count: i32 = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT play_count FROM tracks WHERE id = 'negative'".to_string(),
            ))
            .await
            .expect("query track after rejected migration")
            .expect("legacy track remains")
            .try_get("", "play_count")
            .expect("read unchanged play count");
        assert_eq!(count, -4, "validation must precede legacy repair");
    }

    #[tokio::test]
    async fn migrator_finishes_an_already_applied_schema_change() {
        let db = database_before_playback_history_migration().await;
        insert_track(&db, "legacy", -8).await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("apply schema without migration ledger update");
        assert!(!migration_is_applied(&db).await);
        assert_eq!(track_history(&db, "legacy").await, (0, None));

        db.execute_unprepared("UPDATE tracks SET play_count = -2 WHERE id = 'legacy'")
            .await
            .expect("simulate another legacy write before retry");
        Migration
            .up(&manager)
            .await
            .expect("repeat direct migration");
        Migrator::up(&db, Some(1))
            .await
            .expect("finish playback-history migration");

        assert!(migration_is_applied(&db).await);
        assert_eq!(track_history(&db, "legacy").await, (0, None));
    }

    #[tokio::test]
    async fn down_and_up_round_trip_preserves_tracks_and_resets_timestamp_to_null() {
        let db = database_before_playback_history_migration().await;
        insert_track(&db, "round-trip", 11).await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply playback-history migration");
        db.execute_unprepared(
            "UPDATE tracks SET last_played_at_ms = 1721234567890 \
             WHERE id = 'round-trip'",
        )
        .await
        .expect("record playback timestamp");

        Migrator::down(&db, Some(1))
            .await
            .expect("roll back playback-history migration");
        assert_eq!(playback_column(&db).await, None);

        let count: i32 = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT play_count FROM tracks WHERE id = 'round-trip'".to_string(),
            ))
            .await
            .expect("query track after rollback")
            .expect("track survives rollback")
            .try_get("", "play_count")
            .expect("play count survives rollback");
        assert_eq!(count, 11);

        Migrator::up(&db, Some(1))
            .await
            .expect("reapply playback-history migration");
        assert_eq!(track_history(&db, "round-trip").await, (11, None));

        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("drop playback-history column before ledger update");
        assert!(migration_is_applied(&db).await);

        Migrator::down(&db, Some(1))
            .await
            .expect("finish partially applied playback-history teardown");
        assert!(!migration_is_applied(&db).await);
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("repeat playback-history teardown");
        assert_eq!(playback_column(&db).await, None);
    }
}
