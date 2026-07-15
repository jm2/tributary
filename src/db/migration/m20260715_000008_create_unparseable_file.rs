//! Migration: create the `unparseable_files` table for tracking files that
//! failed to parse during scanning, so they are not re-attempted on every
//! subsequent scan.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(UnparseableFile::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(UnparseableFile::FilePath)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(UnparseableFile::DateModified)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(UnparseableFile::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum UnparseableFile {
    Table,
    FilePath,
    DateModified,
}
