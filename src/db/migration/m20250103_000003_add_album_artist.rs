//! Migration: add `album_artist_name` column to the `tracks` table.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("tracks"))
                    .add_column(
                        ColumnDef::new(Alias::new("album_artist_name"))
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("tracks"))
                    .drop_column(Alias::new("album_artist_name"))
                    .to_owned(),
            )
            .await
    }
}
