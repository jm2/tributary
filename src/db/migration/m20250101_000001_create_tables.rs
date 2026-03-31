//! Initial migration: create the `tracks` table.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Create the tracks table
        manager
            .create_table(
                Table::create()
                    .table(Tracks::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(Tracks::Id).string().not_null().primary_key())
                    .col(ColumnDef::new(Tracks::FilePath).string().not_null().unique_key())
                    .col(ColumnDef::new(Tracks::Title).string().not_null().default(""))
                    .col(ColumnDef::new(Tracks::ArtistName).string().not_null().default(""))
                    .col(ColumnDef::new(Tracks::AlbumTitle).string().not_null().default(""))
                    .col(ColumnDef::new(Tracks::Genre).string().null())
                    .col(ColumnDef::new(Tracks::Year).integer().null())
                    .col(ColumnDef::new(Tracks::TrackNumber).integer().null())
                    .col(ColumnDef::new(Tracks::DiscNumber).integer().null())
                    .col(ColumnDef::new(Tracks::DurationSecs).big_integer().null())
                    .col(ColumnDef::new(Tracks::BitrateKbps).integer().null())
                    .col(ColumnDef::new(Tracks::SampleRateHz).integer().null())
                    .col(ColumnDef::new(Tracks::Format).string().null())
                    .col(ColumnDef::new(Tracks::PlayCount).integer().not_null().default(0))
                    .col(ColumnDef::new(Tracks::DateAdded).string().not_null())
                    .col(ColumnDef::new(Tracks::DateModified).string().not_null())
                    .col(ColumnDef::new(Tracks::FileSizeBytes).big_integer().null())
                    .to_owned(),
            )
            .await?;

        // Indexes for common queries
        manager
            .create_index(
                Index::create()
                    .name("idx_tracks_artist")
                    .table(Tracks::Table)
                    .col(Tracks::ArtistName)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_tracks_album")
                    .table(Tracks::Table)
                    .col(Tracks::AlbumTitle)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_tracks_genre")
                    .table(Tracks::Table)
                    .col(Tracks::Genre)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Tracks::Table).to_owned())
            .await
    }
}

/// Column identifiers for the `tracks` table.
#[derive(DeriveIden)]
enum Tracks {
    Table,
    Id,
    FilePath,
    Title,
    ArtistName,
    AlbumTitle,
    Genre,
    Year,
    TrackNumber,
    DiscNumber,
    DurationSecs,
    BitrateKbps,
    SampleRateHz,
    Format,
    PlayCount,
    DateAdded,
    DateModified,
    FileSizeBytes,
}
