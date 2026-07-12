//! Migration: persist availability and filesystem identity for library roots.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(LibraryRoots::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(LibraryRoots::Path)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(LibraryRoots::DeviceId).string().null())
                    .col(
                        ColumnDef::new(LibraryRoots::IdentityConfirmed)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(LibraryRoots::IsAvailable)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(LibraryRoots::LastScanComplete)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(LibraryRoots::LastCheckedAt)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(LibraryRoots::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum LibraryRoots {
    Table,
    Path,
    DeviceId,
    IdentityConfirmed,
    IsAvailable,
    LastScanComplete,
    LastCheckedAt,
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{ConnectionTrait, Database, DatabaseConnection};

    use super::*;
    use crate::db::migration::Migrator;

    async fn migrated_database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, None).await.expect("apply all migrations");
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
    async fn down_can_be_repeated_after_the_table_is_gone() {
        let db = migrated_database().await;
        let manager = SchemaManager::new(&db);

        Migration
            .down(&manager)
            .await
            .expect("drop library roots table");
        assert!(!manager
            .has_table("library_roots")
            .await
            .expect("inspect library roots table"));
        Migration
            .down(&manager)
            .await
            .expect("repeat library roots teardown");
    }

    #[tokio::test]
    async fn migrator_finishes_a_partially_applied_down() {
        let db = migrated_database().await;
        assert!(migration_is_applied(&db).await);

        // Simulate the table DROP committing before the migration ledger row
        // is removed, then let the normal migrator retry the down operation.
        db.execute_unprepared("DROP TABLE library_roots")
            .await
            .expect("pre-drop library roots table");
        Migrator::down(&db, Some(1))
            .await
            .expect("finish partially applied library roots down");
        assert!(!migration_is_applied(&db).await);

        let manager = SchemaManager::new(&db);
        assert!(!manager
            .has_table("library_roots")
            .await
            .expect("inspect library roots table"));
    }
}
