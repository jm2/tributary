//! Migration: enforce `UNIQUE(playlist_id, position)` on `playlist_entries`.
//!
//! A prior bug could let two entries share a position within the same
//! playlist. Before adding the UNIQUE index we first renumber every
//! playlist's entries to a clean `0..N` sequence (ordered by current
//! position, ties broken by id) so any pre-existing duplicates are
//! resolved and the index can be created safely.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Step 1 — renumber. For each entry, its new position is the count of
        // sibling entries (same playlist) that sort before it under the total
        // order (position, id). That yields a gap-free, duplicate-free
        // `0..N` sequence per playlist. A plain correlated subquery is used
        // (rather than a window function) for the broadest SQLite support.
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE playlist_entries
                 SET position = (
                     SELECT COUNT(*)
                     FROM playlist_entries AS sibling
                     WHERE sibling.playlist_id = playlist_entries.playlist_id
                       AND (
                             sibling.position < playlist_entries.position
                          OR (sibling.position = playlist_entries.position
                              AND sibling.id < playlist_entries.id)
                       )
                 )",
            )
            .await?;

        // Step 2 — now that positions are unique, enforce it.
        manager
            .create_index(
                Index::create()
                    .name("idx_playlist_entries_playlist_position_unique")
                    .table(PlaylistEntries::Table)
                    .col(PlaylistEntries::PlaylistId)
                    .col(PlaylistEntries::Position)
                    .unique()
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_playlist_entries_playlist_position_unique")
                    .table(PlaylistEntries::Table)
                    .to_owned(),
            )
            .await
    }
}

/// Column identifiers for the `playlist_entries` table.
#[derive(DeriveIden)]
enum PlaylistEntries {
    Table,
    PlaylistId,
    Position,
}
