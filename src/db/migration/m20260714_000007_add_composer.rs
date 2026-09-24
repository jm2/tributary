//! Migration: add `composer` column to the `tracks` table.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // SQLite can commit the ALTER before the migration ledger row is
        // written, so a retry must accept the column already being there.
        if manager.has_column("tracks", "composer").await? {
            return Ok(());
        }
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("tracks"))
                    .add_column(ColumnDef::new(Alias::new("composer")).string().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("tracks", "composer").await? {
            return Ok(());
        }
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("tracks"))
                    .drop_column(Alias::new("composer"))
                    .to_owned(),
            )
            .await
    }
}
