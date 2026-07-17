//! Migration: rename table `unparseable_file` to `unparseable_files` for
//! consistent plural naming across the schema.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .rename_table(
                Table::rename()
                    .table(Alias::new("unparseable_file"), Alias::new("unparseable_files"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .rename_table(
                Table::rename()
                    .table(Alias::new("unparseable_files"), Alias::new("unparseable_file"))
                    .to_owned(),
            )
            .await
    }
}
