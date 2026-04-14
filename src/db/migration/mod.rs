//! Migration registry.

use sea_orm_migration::prelude::*;

mod m20250101_000001_create_tables;
mod m20250102_000002_create_playlists;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20250101_000001_create_tables::Migration),
            Box::new(m20250102_000002_create_playlists::Migration),
        ]
    }
}
