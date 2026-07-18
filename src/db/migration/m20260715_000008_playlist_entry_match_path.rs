//! Migration: retain an imported playlist entry's exact source file path.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager
            .has_column("playlist_entries", "match_file_path")
            .await?
        {
            return Ok(());
        }

        manager
            .alter_table(
                Table::alter()
                    .table(PlaylistEntries::Table)
                    .add_column(
                        ColumnDef::new(PlaylistEntries::MatchFilePath)
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager
            .has_column("playlist_entries", "match_file_path")
            .await?
        {
            return Ok(());
        }

        manager
            .alter_table(
                Table::alter()
                    .table(PlaylistEntries::Table)
                    .drop_column(PlaylistEntries::MatchFilePath)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum PlaylistEntries {
    Table,
    MatchFilePath,
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_before_match_path_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(8))
            .await
            .expect("apply migrations preceding playlist entry match path");
        db
    }

    async fn insert_playlist_entry(db: &DatabaseConnection) {
        db.execute_unprepared(
            "INSERT INTO playlists (id, name, created_at, updated_at)
             VALUES ('playlist-1', 'Imported',
                     '2026-07-15T00:00:00Z', '2026-07-15T00:00:00Z')",
        )
        .await
        .expect("insert playlist");
        db.execute_unprepared(
            "INSERT INTO playlist_entries (
                 id, playlist_id, position, match_title, match_artist,
                 match_album, match_duration_secs
             )
             VALUES ('entry-1', 'playlist-1', 0, 'Title', 'Artist', 'Album', 181)",
        )
        .await
        .expect("insert pre-migration playlist entry");
    }

    async fn match_path_nullable(db: &DatabaseConnection) -> Option<bool> {
        db.query_all(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA table_info('playlist_entries')".to_string(),
        ))
        .await
        .expect("inspect playlist entry columns")
        .into_iter()
        .find_map(|row| {
            let name: String = row.try_get("", "name").ok()?;
            (name == "match_file_path").then(|| {
                row.try_get::<i32>("", "notnull")
                    .expect("match path nullability")
                    == 0
            })
        })
    }

    async fn entry_values(
        db: &DatabaseConnection,
    ) -> (String, i32, String, String, String, Option<i32>) {
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT playlist_id, position, match_title, match_artist,
                        match_album, match_duration_secs
                 FROM playlist_entries
                 WHERE id = 'entry-1'"
                    .to_string(),
            ))
            .await
            .expect("query playlist entry")
            .expect("playlist entry exists");
        (
            row.try_get("", "playlist_id").expect("playlist id"),
            row.try_get("", "position").expect("position"),
            row.try_get("", "match_title").expect("match title"),
            row.try_get("", "match_artist").expect("match artist"),
            row.try_get("", "match_album").expect("match album"),
            row.try_get("", "match_duration_secs")
                .expect("match duration"),
        )
    }

    #[tokio::test]
    async fn up_adds_a_nullable_path_without_changing_existing_entries() {
        let db = database_before_match_path_migration().await;
        insert_playlist_entry(&db).await;

        Migrator::up(&db, Some(1))
            .await
            .expect("apply playlist entry match path migration");

        assert_eq!(match_path_nullable(&db).await, Some(true));
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT match_file_path FROM playlist_entries WHERE id = 'entry-1'".to_string(),
            ))
            .await
            .expect("query imported path")
            .expect("playlist entry exists");
        assert_eq!(
            row.try_get::<Option<String>>("", "match_file_path")
                .expect("match file path"),
            None
        );
        assert_eq!(
            entry_values(&db).await,
            (
                "playlist-1".to_string(),
                0,
                "Title".to_string(),
                "Artist".to_string(),
                "Album".to_string(),
                Some(181),
            )
        );
    }

    #[tokio::test]
    async fn down_drops_only_the_imported_path_and_can_be_retried() {
        let db = database_before_match_path_migration().await;
        insert_playlist_entry(&db).await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply playlist entry match path migration");
        db.execute_unprepared(
            "UPDATE playlist_entries
             SET match_file_path = '/imported/library/song.flac'
             WHERE id = 'entry-1'",
        )
        .await
        .expect("save imported path");
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT match_file_path FROM playlist_entries WHERE id = 'entry-1'".to_string(),
            ))
            .await
            .expect("query imported path")
            .expect("playlist entry exists");
        assert_eq!(
            row.try_get::<Option<String>>("", "match_file_path")
                .expect("match file path")
                .as_deref(),
            Some("/imported/library/song.flac")
        );

        Migrator::down(&db, Some(1))
            .await
            .expect("roll back playlist entry match path migration");
        assert_eq!(match_path_nullable(&db).await, None);
        assert_eq!(
            entry_values(&db).await,
            (
                "playlist-1".to_string(),
                0,
                "Title".to_string(),
                "Artist".to_string(),
                "Album".to_string(),
                Some(181),
            )
        );

        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("repeat match path teardown");
        assert_eq!(match_path_nullable(&db).await, None);
    }
}
