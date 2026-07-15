//! Migration: persist completed library-root reauthorization receipts.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::Statement;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(RootReauthorizationReceipts::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(RootReauthorizationReceipts::RequestId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RootReauthorizationReceipts::OldPath)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RootReauthorizationReceipts::NewPath)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RootReauthorizationReceipts::MarkerIdentity)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RootReauthorizationReceipts::CompletedAt)
                            .text()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        validate_receipt_table(manager).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(RootReauthorizationReceipts::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

/// Require the exact receipt shape instead of allowing `IF NOT EXISTS` to
/// silently accept a conflicting table after an interrupted migration.
async fn validate_receipt_table(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let connection = manager.get_connection();
    let backend = manager.get_database_backend();

    let object = connection
        .query_one(Statement::from_string(
            backend,
            "SELECT type FROM sqlite_master
             WHERE name = 'root_reauthorization_receipts'"
                .to_string(),
        ))
        .await?;
    let Some(object) = object else {
        return Err(DbErr::Migration(
            "root_reauthorization_receipts was not created".to_string(),
        ));
    };
    let object_type: String = object.try_get("", "type")?;
    if object_type != "table" {
        return Err(DbErr::Migration(format!(
            "root_reauthorization_receipts must be a table, found {object_type}"
        )));
    }

    let columns = connection
        .query_all(Statement::from_string(
            backend,
            "PRAGMA table_info('root_reauthorization_receipts')".to_string(),
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
    let expected = vec![
        ("request_id".to_string(), "text".to_string(), 1, None, 1),
        ("old_path".to_string(), "text".to_string(), 1, None, 0),
        ("new_path".to_string(), "text".to_string(), 1, None, 0),
        (
            "marker_identity".to_string(),
            "text".to_string(),
            1,
            None,
            0,
        ),
        ("completed_at".to_string(), "text".to_string(), 1, None, 0),
    ];

    if columns != expected {
        return Err(DbErr::Migration(format!(
            "root_reauthorization_receipts has an unexpected schema: {columns:?}"
        )));
    }

    Ok(())
}

#[derive(DeriveIden)]
enum RootReauthorizationReceipts {
    Table,
    RequestId,
    OldPath,
    NewPath,
    MarkerIdentity,
    CompletedAt,
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, Set,
    };

    use super::*;
    use crate::db::entities::root_reauthorization_receipt;
    use crate::db::migration::Migrator;

    async fn database_before_receipt_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(8))
            .await
            .expect("apply migrations preceding reauthorization receipts");
        db
    }

    async fn migrated_database() -> DatabaseConnection {
        let db = database_before_receipt_migration().await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply reauthorization receipt migration");
        db
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
    async fn up_creates_the_exact_schema_and_can_be_retried() {
        let db = database_before_receipt_migration().await;
        let manager = SchemaManager::new(&db);

        Migration.up(&manager).await.expect("create receipt table");
        validate_receipt_table(&manager)
            .await
            .expect("validate receipt table");
        Migration.up(&manager).await.expect("repeat receipt setup");
    }

    #[tokio::test]
    async fn up_rejects_a_conflicting_existing_schema() {
        let db = database_before_receipt_migration().await;
        db.execute_unprepared(
            "CREATE TABLE root_reauthorization_receipts (
                 request_id TEXT PRIMARY KEY NOT NULL
             )",
        )
        .await
        .expect("create conflicting receipt table");

        assert!(
            Migration.up(&SchemaManager::new(&db)).await.is_err(),
            "a partial receipt schema must fail closed"
        );
    }

    #[tokio::test]
    async fn entity_round_trip_preserves_every_receipt_field() {
        let db = migrated_database().await;
        let expected = root_reauthorization_receipt::Model {
            request_id: "request-1".to_string(),
            old_path: "/legacy/Music".to_string(),
            new_path: "/run/user/1000/doc/portal/Music".to_string(),
            marker_identity: "marker-identity-1".to_string(),
            completed_at: "2026-07-15T18:45:00Z".to_string(),
        };

        root_reauthorization_receipt::ActiveModel {
            request_id: Set(expected.request_id.clone()),
            old_path: Set(expected.old_path.clone()),
            new_path: Set(expected.new_path.clone()),
            marker_identity: Set(expected.marker_identity.clone()),
            completed_at: Set(expected.completed_at.clone()),
        }
        .insert(&db)
        .await
        .expect("insert receipt");

        assert_eq!(
            root_reauthorization_receipt::Entity::find_by_id("request-1")
                .one(&db)
                .await
                .expect("load receipt"),
            Some(expected.clone())
        );

        let repeated_paths = root_reauthorization_receipt::ActiveModel {
            request_id: Set("request-2".to_string()),
            old_path: Set(expected.old_path.clone()),
            new_path: Set(expected.new_path.clone()),
            marker_identity: Set(expected.marker_identity.clone()),
            completed_at: Set(expected.completed_at.clone()),
        };
        repeated_paths
            .insert(&db)
            .await
            .expect("a later request may reauthorize the same root again");

        let conflicting_request = root_reauthorization_receipt::ActiveModel {
            request_id: Set(expected.request_id),
            old_path: Set("/different/old".to_string()),
            new_path: Set("/different/new".to_string()),
            marker_identity: Set("different-marker".to_string()),
            completed_at: Set("2026-07-15T18:46:00Z".to_string()),
        };
        assert!(
            conflicting_request.insert(&db).await.is_err(),
            "one request id cannot silently acquire different receipt fields"
        );
    }

    #[tokio::test]
    async fn down_and_up_are_repeatable() {
        let db = migrated_database().await;
        let manager = SchemaManager::new(&db);

        Migration.down(&manager).await.expect("drop receipt table");
        Migration
            .down(&manager)
            .await
            .expect("repeat receipt teardown");
        assert!(!manager
            .has_table("root_reauthorization_receipts")
            .await
            .expect("inspect receipt table"));

        Migration
            .up(&manager)
            .await
            .expect("recreate receipt table");
        Migration.up(&manager).await.expect("repeat receipt setup");
        validate_receipt_table(&manager)
            .await
            .expect("validate recreated receipt table");
    }

    #[tokio::test]
    async fn migrator_finishes_a_partially_applied_up() {
        let db = database_before_receipt_migration().await;
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("create receipt table without updating migration ledger");
        assert!(!migration_is_applied(&db).await);

        Migrator::up(&db, Some(1))
            .await
            .expect("finish partially applied receipt migration");
        assert!(migration_is_applied(&db).await);
    }

    #[tokio::test]
    async fn migrator_finishes_a_partially_applied_down() {
        let db = migrated_database().await;
        assert!(migration_is_applied(&db).await);

        db.execute_unprepared("DROP TABLE root_reauthorization_receipts")
            .await
            .expect("pre-drop receipt table");
        Migrator::down(&db, Some(1))
            .await
            .expect("finish partially applied receipt teardown");
        assert!(!migration_is_applied(&db).await);
        assert!(!SchemaManager::new(&db)
            .has_table("root_reauthorization_receipts")
            .await
            .expect("inspect receipt table"));
    }
}
