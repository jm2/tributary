//! Playlist migration: create `playlists` and `playlist_entries` tables.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ── playlists table ─────────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(Playlists::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Playlists::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Playlists::Name).string().not_null())
                    .col(
                        ColumnDef::new(Playlists::IsSmart)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(ColumnDef::new(Playlists::SmartRulesJson).string().null())
                    .col(
                        ColumnDef::new(Playlists::LimitEnabled)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(ColumnDef::new(Playlists::LimitValue).integer().null())
                    .col(ColumnDef::new(Playlists::LimitUnit).string().null())
                    .col(ColumnDef::new(Playlists::LimitSort).string().null())
                    .col(
                        ColumnDef::new(Playlists::MatchMode)
                            .string()
                            .not_null()
                            .default("all"),
                    )
                    .col(
                        ColumnDef::new(Playlists::LiveUpdating)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(ColumnDef::new(Playlists::CreatedAt).string().not_null())
                    .col(ColumnDef::new(Playlists::UpdatedAt).string().not_null())
                    .to_owned(),
            )
            .await?;

        // ── playlist_entries table ───────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(PlaylistEntries::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PlaylistEntries::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(PlaylistEntries::PlaylistId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PlaylistEntries::Position)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(PlaylistEntries::TrackId).string().null())
                    .col(
                        ColumnDef::new(PlaylistEntries::MatchTitle)
                            .string()
                            .not_null()
                            .default(""),
                    )
                    .col(
                        ColumnDef::new(PlaylistEntries::MatchArtist)
                            .string()
                            .not_null()
                            .default(""),
                    )
                    .col(
                        ColumnDef::new(PlaylistEntries::MatchAlbum)
                            .string()
                            .not_null()
                            .default(""),
                    )
                    .col(
                        ColumnDef::new(PlaylistEntries::MatchDurationSecs)
                            .integer()
                            .null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_entry_playlist")
                            .from(PlaylistEntries::Table, PlaylistEntries::PlaylistId)
                            .to(Playlists::Table, Playlists::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── Indexes ─────────────────────────────────────────────────
        manager
            .create_index(
                Index::create()
                    .name("idx_playlist_entries_playlist_id")
                    .table(PlaylistEntries::Table)
                    .col(PlaylistEntries::PlaylistId)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_playlist_entries_track_id")
                    .table(PlaylistEntries::Table)
                    .col(PlaylistEntries::TrackId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PlaylistEntries::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Playlists::Table).to_owned())
            .await
    }
}

/// Column identifiers for the `playlists` table.
#[derive(DeriveIden)]
enum Playlists {
    Table,
    Id,
    Name,
    IsSmart,
    SmartRulesJson,
    LimitEnabled,
    LimitValue,
    LimitUnit,
    LimitSort,
    MatchMode,
    LiveUpdating,
    CreatedAt,
    UpdatedAt,
}

/// Column identifiers for the `playlist_entries` table.
#[derive(DeriveIden)]
enum PlaylistEntries {
    Table,
    Id,
    PlaylistId,
    Position,
    TrackId,
    MatchTitle,
    MatchArtist,
    MatchAlbum,
    MatchDurationSecs,
}
