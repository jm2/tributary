//! Database connection factory and migration runner.

use sea_orm::{Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;
use tracing::info;

use super::migration::Migrator;

/// Initialise the SQLite database.
///
/// Creates the data directory and database file if they don't exist,
/// then runs all pending migrations.
pub async fn init_db() -> Result<DatabaseConnection, DbErr> {
    let data_dir = dirs::data_dir()
        .expect("Could not determine XDG data directory")
        .join("tributary");

    std::fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    let db_path = data_dir.join("library.db");
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());

    info!(path = %db_path.display(), "Opening database");
    let db = Database::connect(&db_url).await?;

    info!("Running pending migrations");
    Migrator::up(&db, None).await?;

    info!("Database ready");
    Ok(db)
}
