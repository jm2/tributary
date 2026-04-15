//! Database connection factory and migration runner.

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;
use tokio::sync::OnceCell;
use tracing::info;

use super::migration::Migrator;

/// Shared database connection — initialised once, reused everywhere.
///
/// Using a `OnceCell` ensures that `init_db()` only runs migrations and
/// opens the SQLite file a single time.  Subsequent callers get a cheap
/// clone of the same connection (SeaORM's `DatabaseConnection` is
/// internally `Arc`-wrapped and safe to share across tasks).
static SHARED_DB: OnceCell<DatabaseConnection> = OnceCell::const_new();

/// Obtain the shared database connection, initialising it on first call.
///
/// This is the preferred entry point for all code that needs DB access.
/// The first invocation opens the SQLite file, enables WAL mode, and
/// runs pending migrations.  Every subsequent call returns instantly.
pub async fn get_or_init_db() -> Result<DatabaseConnection, DbErr> {
    let db = SHARED_DB
        .get_or_try_init(|| async {
            let data_dir = dirs::data_dir()
                .expect("Could not determine XDG data directory")
                .join("tributary");

            std::fs::create_dir_all(&data_dir).expect("Failed to create data directory");

            let db_path = data_dir.join("library.db");
            let db_url = format!("sqlite://{}?mode=rwc", db_path.display());

            info!(path = %db_path.display(), "Opening database");
            let db = Database::connect(&db_url).await?;

            // Enable WAL mode for better concurrent read/write performance.
            // WAL allows readers to proceed without blocking on writers and
            // significantly reduces SQLITE_BUSY errors under load.
            db.execute_unprepared("PRAGMA journal_mode=WAL").await?;
            db.execute_unprepared("PRAGMA busy_timeout=5000").await?;

            info!("Running pending migrations");
            Migrator::up(&db, None).await?;

            info!("Database ready");
            Ok::<DatabaseConnection, DbErr>(db)
        })
        .await?;
    Ok(db.clone())
}

/// Initialise the SQLite database.
///
/// Creates the data directory and database file if they don't exist,
/// then runs all pending migrations.
///
/// **Prefer [`get_or_init_db`] instead** — this function is retained
/// for backward compatibility but now delegates to the shared pool.
pub async fn init_db() -> Result<DatabaseConnection, DbErr> {
    get_or_init_db().await
}
