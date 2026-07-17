//! Migration registry.

use sea_orm_migration::prelude::*;

mod m20250101_000001_create_tables;
mod m20250102_000002_create_playlists;
mod m20250103_000003_add_album_artist;
mod m20250104_000004_unique_entry_position;
mod m20250710_000005_create_library_roots;
mod m20260712_000006_playlist_track_fk;
mod m20260714_000007_add_composer;
mod m20260715_000008_create_unparseable_file;

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
            Box::new(m20260715_000008_create_unparseable_file::Migration),
        ]
    }
}
