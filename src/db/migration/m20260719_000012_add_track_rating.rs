//! Migration: add Tributary-owned ratings to local tracks.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

const ADD_RATING_SQL: &str = "ALTER TABLE tracks ADD COLUMN rating INTEGER NULL \
    CHECK (rating IS NULL OR (typeof(rating) = 'integer' AND rating BETWEEN 1 AND 100))";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("tracks", "rating").await? {
            manager
                .get_connection()
                .execute_unprepared(ADD_RATING_SQL)
                .await?;
        }

        // SQLite may commit ALTER TABLE before SeaORM records this migration.
        // Accept a retry only when the existing column has the complete
        // canonical shape, including its whole-integer range constraint.
        validate_rating_column(manager).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("tracks", "rating").await? {
            return Ok(());
        }

        manager
            .alter_table(
                Table::alter()
                    .table(Tracks::Table)
                    .drop_column(Tracks::Rating)
                    .to_owned(),
            )
            .await
    }
}

async fn validate_rating_column(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            "PRAGMA table_info('tracks')".to_string(),
        ))
        .await?;

    let Some(column) = columns.into_iter().find(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|name| name == "rating")
    }) else {
        return Err(DbErr::Migration(
            "tracks.rating was not created".to_string(),
        ));
    };

    let column_type: String = column.try_get("", "type")?;
    let not_null: i32 = column.try_get("", "notnull")?;
    let default: Option<String> = column.try_get("", "dflt_value")?;
    let primary_key: i32 = column.try_get("", "pk")?;
    if !column_type.eq_ignore_ascii_case("integer")
        || not_null != 0
        || default.is_some()
        || primary_key != 0
    {
        return Err(DbErr::Migration(format!(
            "tracks.rating has an unexpected schema: type={column_type:?}, \
             not_null={not_null}, default={default:?}, primary_key={primary_key}"
        )));
    }

    let table_row = manager
        .get_connection()
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'tracks'".to_string(),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("tracks table definition is missing".to_string()))?;
    let table_sql: String = table_row.try_get("", "sql")?;
    let normalized: String = table_sql
        .chars()
        .filter(|character| {
            !character.is_ascii_whitespace() && !matches!(character, '"' | '`' | '[' | ']')
        })
        .flat_map(char::to_lowercase)
        .collect();
    // Require the exact normalized definition between real column boundaries.
    // The comma case keeps retry validation valid after a later migration has
    // appended another column; the closing-parenthesis case covers rating as
    // the current final column. An extra same-column restriction (for example
    // `CHECK (rating <= 50)`) has neither boundary and is still rejected.
    const REQUIRED_COLUMN_DEFINITION: &str =
        "ratingintegernullcheck(ratingisnullor(typeof(rating)='integer'andratingbetween1and100))";
    let followed_by_column = format!(",{REQUIRED_COLUMN_DEFINITION},");
    let final_column = format!(",{REQUIRED_COLUMN_DEFINITION})");
    if !normalized.contains(&followed_by_column) && !normalized.ends_with(&final_column) {
        return Err(DbErr::Migration(
            "tracks.rating does not have the exact canonical whole-integer 1..=100 CHECK constraint"
                .to_string(),
        ));
    }

    Ok(())
}

#[derive(DeriveIden)]
enum Tracks {
    Table,
    Rating,
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    };

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_before_rating_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(11))
            .await
            .expect("apply migrations preceding ratings");
        db
    }

    async fn insert_legacy_track(db: &DatabaseConnection, id: &str) {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO tracks (
                 id, file_path, title, artist_name, album_title, play_count,
                 date_added, date_modified
             ) VALUES (?, ?, 'Title', 'Artist', 'Album', 7, 'added', 'modified')",
            [id.into(), format!("/music/{id}.flac").into()],
        ))
        .await
        .expect("insert legacy track");
    }

    async fn rating_value(db: &DatabaseConnection, id: &str) -> Option<i32> {
        db.query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT rating FROM tracks WHERE id = ?",
            [id.into()],
        ))
        .await
        .expect("query track")
        .expect("track exists")
        .try_get("", "rating")
        .expect("read rating")
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
    async fn up_adds_nullable_constrained_integer_and_leaves_legacy_rows_unrated() {
        let db = database_before_rating_migration().await;
        insert_legacy_track(&db, "legacy-one").await;
        insert_legacy_track(&db, "legacy-two").await;

        Migrator::up(&db, Some(1))
            .await
            .expect("apply ratings migration");

        assert_eq!(rating_value(&db, "legacy-one").await, None);
        assert_eq!(rating_value(&db, "legacy-two").await, None);

        for valid in [1, 100] {
            db.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE tracks SET rating = ? WHERE id = 'legacy-one'",
                [valid.into()],
            ))
            .await
            .expect("boundary rating is valid");
            assert_eq!(rating_value(&db, "legacy-one").await, Some(valid));
        }
        db.execute_unprepared("UPDATE tracks SET rating = NULL WHERE id = 'legacy-one'")
            .await
            .expect("unrated is valid");
        assert_eq!(rating_value(&db, "legacy-one").await, None);
    }

    #[tokio::test]
    async fn database_rejects_out_of_range_and_non_integer_storage() {
        let db = database_before_rating_migration().await;
        insert_legacy_track(&db, "constrained").await;
        Migrator::up(&db, Some(1)).await.expect("migrate");

        for invalid in [
            "UPDATE tracks SET rating = 0 WHERE id = 'constrained'",
            "UPDATE tracks SET rating = 101 WHERE id = 'constrained'",
            "UPDATE tracks SET rating = -1 WHERE id = 'constrained'",
            "UPDATE tracks SET rating = 50.5 WHERE id = 'constrained'",
            "UPDATE tracks SET rating = 'not-a-rating' WHERE id = 'constrained'",
        ] {
            db.execute_unprepared(invalid)
                .await
                .expect_err("invalid rating must violate the schema constraint");
            assert_eq!(rating_value(&db, "constrained").await, None);
        }

        // SQLite's INTEGER affinity canonicalizes a whole-valued numeric
        // input before the CHECK observes it.
        db.execute_unprepared("UPDATE tracks SET rating = 50.0 WHERE id = 'constrained'")
            .await
            .expect("whole numeric input canonicalizes to INTEGER");
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT rating, typeof(rating) AS storage FROM tracks WHERE id = 'constrained'"
                    .to_string(),
            ))
            .await
            .expect("query canonical storage")
            .expect("track exists");
        assert_eq!(row.try_get::<i32>("", "rating").unwrap(), 50);
        assert_eq!(row.try_get::<String>("", "storage").unwrap(), "integer");
    }

    #[tokio::test]
    async fn interrupted_retry_accepts_only_the_exact_compatible_shape() {
        let compatible = database_before_rating_migration().await;
        compatible
            .execute_unprepared(ADD_RATING_SQL)
            .await
            .expect("install compatible interrupted column");
        Migrator::up(&compatible, Some(1))
            .await
            .expect("accept compatible interrupted migration");
        assert!(migration_is_applied(&compatible).await);

        let followed_by_later_column = database_before_rating_migration().await;
        followed_by_later_column
            .execute_unprepared(ADD_RATING_SQL)
            .await
            .expect("install compatible interrupted column");
        followed_by_later_column
            .execute_unprepared("ALTER TABLE tracks ADD COLUMN later_marker TEXT NULL")
            .await
            .expect("simulate a later appended schema column");
        Migrator::up(&followed_by_later_column, Some(1))
            .await
            .expect("accept the exact rating definition before a later column");
        assert!(migration_is_applied(&followed_by_later_column).await);

        let incompatible = database_before_rating_migration().await;
        incompatible
            .execute_unprepared("ALTER TABLE tracks ADD COLUMN rating INTEGER NULL")
            .await
            .expect("install unconstrained lookalike");
        let error = Migrator::up(&incompatible, Some(1))
            .await
            .expect_err("unconstrained rating column must fail closed");
        assert!(error.to_string().contains("CHECK constraint"));
        assert!(!migration_is_applied(&incompatible).await);

        let restrictive = database_before_rating_migration().await;
        restrictive
            .execute_unprepared(
                "ALTER TABLE tracks ADD COLUMN rating INTEGER NULL \
                 CHECK (rating IS NULL OR (typeof(rating) = 'integer' AND rating BETWEEN 1 AND 100)) \
                 CHECK (rating IS NULL OR rating <= 50)",
            )
            .await
            .expect("install restrictive lookalike");
        let error = Migrator::up(&restrictive, Some(1))
            .await
            .expect_err("extra restrictive rating constraint must fail closed");
        assert!(error.to_string().contains("exact canonical"));
        assert!(!migration_is_applied(&restrictive).await);
    }

    #[tokio::test]
    async fn down_and_repeated_up_down_are_safe() {
        let db = database_before_rating_migration().await;
        insert_legacy_track(&db, "retry").await;
        Migrator::up(&db, Some(1)).await.expect("first up");
        db.execute_unprepared("UPDATE tracks SET rating = 64 WHERE id = 'retry'")
            .await
            .expect("rate track");

        Migrator::down(&db, Some(1)).await.expect("first down");
        assert!(!SchemaManager::new(&db)
            .has_column("tracks", "rating")
            .await
            .unwrap());
        Migration.down(&SchemaManager::new(&db)).await.unwrap();

        Migrator::up(&db, Some(1)).await.expect("second up");
        assert_eq!(rating_value(&db, "retry").await, None);
        Migrator::down(&db, Some(1)).await.expect("second down");
    }
}
