//! Database connection factory and migration runner.

use std::path::Path;
use std::time::Duration;

use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{DatabaseConnection, DbErr, SqlxSqliteConnector};
use sea_orm_migration::MigratorTrait;
use tokio::sync::OnceCell;
use tracing::info;

use super::migration::Migrator;

/// How long a statement waits for a competing writer before failing busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared database connection — initialised once, reused everywhere.
///
/// Using a `OnceCell` ensures that `init_db()` only runs migrations and
/// opens the SQLite file a single time.  Subsequent callers get a cheap
/// clone of the same connection (SeaORM's `DatabaseConnection` is
/// internally `Arc`-wrapped and safe to share across tasks).
static SHARED_DB: OnceCell<DatabaseConnection> = OnceCell::const_new();

/// Settings applied to *every* connection in the pool.
///
/// All three are stated explicitly rather than inherited:
///
/// - `foreign_keys` is what makes `playlist_entries.track_id`'s
///   `ON DELETE SET NULL` fire when a track row is deleted. SQLite defaults
///   this to **off**; sqlx happens to enable it on every connection it opens,
///   and SeaORM never touches it either way. Leaving the library's whole
///   playlist-integrity guarantee resting on an upstream default means a
///   change to that default would silently void the foreign key instead of
///   failing loudly, so we set it ourselves.
/// - `busy_timeout` is per-connection, and `journal_mode` is a file-level
///   setting. Applying them here covers every pooled connection rather than
///   only the one connection that happened to be borrowed at startup.
fn sqlite_connect_options(db_path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(BUSY_TIMEOUT)
}

/// Open a connection pool against `db_path` and run every pending migration.
async fn connect_and_migrate(db_path: &Path) -> Result<DatabaseConnection, DbErr> {
    let pool = SqlitePoolOptions::new()
        .connect_with(sqlite_connect_options(db_path))
        .await
        .map_err(|e| DbErr::Custom(format!("Failed to open database: {e}")))?;
    let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);

    info!("Running pending migrations");
    Migrator::up(&db, None).await?;

    Ok(db)
}

/// Obtain the shared database connection, initialising it on first call.
///
/// This is the preferred entry point for all code that needs DB access.
/// The first invocation opens the SQLite file, enables WAL mode, and
/// runs pending migrations.  Every subsequent call returns instantly.
pub async fn get_or_init_db() -> Result<DatabaseConnection, DbErr> {
    let db = SHARED_DB
        .get_or_try_init(|| async {
            // Return errors instead of panicking: callers wrap this in
            // graceful `match init_db() { Err(e) => … }` handling, and a
            // panic inside this spawned task would be swallowed by tokio,
            // silently killing the library engine with no user feedback.
            let data_dir = dirs::data_dir()
                .ok_or_else(|| DbErr::Custom("Could not determine data directory".into()))?
                .join("tributary");

            std::fs::create_dir_all(&data_dir)
                .map_err(|e| DbErr::Custom(format!("Failed to create data directory: {e}")))?;

            let db_path = data_dir.join("library.db");
            info!(path = %db_path.display(), "Opening database");

            let db = connect_and_migrate(&db_path).await?;

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sea_orm::{
        ActiveModelTrait, ConnectionTrait, DatabaseBackend, EntityTrait, Set, Statement,
        TransactionTrait,
    };
    use uuid::Uuid;

    use super::*;
    use crate::db::entities::{playlist, playlist_entry, track};

    /// A file-backed database, because an in-memory SQLite pool would give
    /// each connection its own empty database and defeat the whole point.
    struct TestDatabase {
        path: PathBuf,
    }

    impl TestDatabase {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("tributary-db-{label}-{}.sqlite", Uuid::new_v4()));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            for sidecar in ["-wal", "-shm"] {
                let mut path = self.path.clone().into_os_string();
                path.push(sidecar);
                let _ = std::fs::remove_file(PathBuf::from(path));
            }
        }
    }

    async fn foreign_keys_enabled(conn: &impl ConnectionTrait) -> bool {
        let row = conn
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA foreign_keys",
            ))
            .await
            .expect("query foreign_keys pragma")
            .expect("foreign_keys pragma returns a row");
        row.try_get::<i32>("", "foreign_keys")
            .expect("foreign_keys pragma value")
            == 1
    }

    fn a_track(id: &str, path: &str) -> track::ActiveModel {
        track::ActiveModel {
            id: Set(id.to_string()),
            file_path: Set(path.to_string()),
            title: Set("Title".to_string()),
            artist_name: Set("Artist".to_string()),
            album_title: Set("Album".to_string()),
            play_count: Set(0),
            date_added: Set("2026-07-13T00:00:00+00:00".to_string()),
            date_modified: Set("2026-07-13T00:00:00+00:00".to_string()),
            ..Default::default()
        }
    }

    /// The pool, not just the first connection borrowed at startup, must
    /// enforce foreign keys — otherwise `ON DELETE SET NULL` fires or not
    /// depending on which connection happens to serve the delete.
    #[tokio::test]
    async fn every_pooled_connection_enforces_foreign_keys() {
        let file = TestDatabase::new("pragma");
        let db = connect_and_migrate(file.path())
            .await
            .expect("open database");

        // Each open transaction pins a distinct pooled connection, so holding
        // several at once forces the pool to hand out more than one.
        let mut transactions = Vec::new();
        for _ in 0..4 {
            transactions.push(db.begin().await.expect("begin transaction"));
        }
        for transaction in &transactions {
            assert!(foreign_keys_enabled(transaction).await);
        }
        for transaction in transactions {
            transaction.rollback().await.expect("rollback transaction");
        }
    }

    /// P1.1's guarantee, asserted end to end against a real pool: deleting a
    /// track must null the referencing playlist entry rather than orphan it or
    /// fail the delete.
    #[tokio::test]
    async fn deleting_a_track_nulls_its_playlist_entry() {
        let file = TestDatabase::new("set-null");
        let db = connect_and_migrate(file.path())
            .await
            .expect("open database");

        playlist::ActiveModel {
            id: Set("playlist-1".to_string()),
            name: Set("Playlist".to_string()),
            is_smart: Set(false),
            limit_enabled: Set(false),
            match_mode: Set("all".to_string()),
            live_updating: Set(true),
            created_at: Set("2026-07-13T00:00:00+00:00".to_string()),
            updated_at: Set("2026-07-13T00:00:00+00:00".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert playlist");

        a_track("track-1", "/music/one.flac")
            .insert(&db)
            .await
            .expect("insert track");

        playlist_entry::ActiveModel {
            id: Set("entry-1".to_string()),
            playlist_id: Set("playlist-1".to_string()),
            position: Set(0),
            track_id: Set(Some("track-1".to_string())),
            match_title: Set("Title".to_string()),
            match_artist: Set("Artist".to_string()),
            match_album: Set("Album".to_string()),
            match_duration_secs: Set(None),
            match_file_path: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert playlist entry");

        track::Entity::delete_by_id("track-1".to_string())
            .exec(&db)
            .await
            .expect("delete track");

        let entry = playlist_entry::Entity::find_by_id("entry-1".to_string())
            .one(&db)
            .await
            .expect("load playlist entry")
            .expect("playlist entry survives the delete");
        assert_eq!(entry.track_id, None);
        assert_eq!(entry.position, 0);
        assert_eq!(entry.match_title, "Title");
    }
}
