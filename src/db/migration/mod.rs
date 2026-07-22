//! Migration registry.

use sea_orm_migration::prelude::*;

mod m20250101_000001_create_tables;
mod m20250102_000002_create_playlists;
mod m20250103_000003_add_album_artist;
mod m20250104_000004_unique_entry_position;
mod m20250710_000005_create_library_roots;
mod m20260712_000006_playlist_track_fk;
mod m20260714_000007_add_composer;
mod m20260715_000008_playlist_entry_match_path;
mod m20260715_000009_create_root_reauthorization_receipts;
mod m20260718_000010_add_playback_history;
mod m20260718_000011_migrate_default_history_playlists;
mod m20260719_000012_add_track_rating;
mod m20260719_000013_source_scoped_playlist_entries;
mod m20260719_000014_server_playlist_links;
mod m20260720_000015_playlist_sidebar_revision;
mod m20260720_000016_rhythmbox_import_receipts;
mod m20260720_000017_lastfm_scrobble_queue;
mod m20260721_000018_lastfm_delivery_pause;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20250101_000001_create_tables::Migration),
            Box::new(m20250102_000002_create_playlists::Migration),
            Box::new(m20250103_000003_add_album_artist::Migration),
            Box::new(m20250104_000004_unique_entry_position::Migration),
            Box::new(m20250710_000005_create_library_roots::Migration),
            Box::new(m20260712_000006_playlist_track_fk::Migration),
            Box::new(m20260714_000007_add_composer::Migration),
            Box::new(m20260715_000008_playlist_entry_match_path::Migration),
            Box::new(m20260715_000009_create_root_reauthorization_receipts::Migration),
            Box::new(m20260718_000010_add_playback_history::Migration),
            Box::new(m20260718_000011_migrate_default_history_playlists::Migration),
            Box::new(m20260719_000012_add_track_rating::Migration),
            Box::new(m20260719_000013_source_scoped_playlist_entries::Migration),
            Box::new(m20260719_000014_server_playlist_links::Migration),
            Box::new(m20260720_000015_playlist_sidebar_revision::Migration),
            Box::new(m20260720_000016_rhythmbox_import_receipts::Migration),
            Box::new(m20260720_000017_lastfm_scrobble_queue::Migration),
            Box::new(m20260721_000018_lastfm_delivery_pause::Migration),
        ]
    }
}

/// Revalidate mutable schema objects whose absence would silently violate
/// runtime correctness even when the migration ledger is fully current.
pub async fn revalidate_critical_objects(
    db: &sea_orm_migration::sea_orm::DatabaseConnection,
) -> Result<(), DbErr> {
    m20260720_000015_playlist_sidebar_revision::revalidate(db).await?;
    m20260720_000016_rhythmbox_import_receipts::revalidate(db).await?;
    m20260720_000017_lastfm_scrobble_queue::revalidate(db).await?;
    m20260721_000018_lastfm_delivery_pause::revalidate(db).await
}
