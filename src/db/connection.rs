//! Database connection factory and migration runner.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{DatabaseConnection, DbErr, SqlxSqliteConnector};
use sea_orm_migration::MigratorTrait;
use tokio::sync::OnceCell;
use tracing::{info, warn};

use super::migration::{self, Migrator};
use super::upgrade::{self, LedgerState};

/// How long a statement waits for a competing writer before failing busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared database connection, or the reason it could not be opened.
///
/// Initialised once per process. A failure is kept as well: every later
/// caller gets the same error instead of reopening the file and re-running
/// the migrations that already failed. SeaORM's `DatabaseConnection` is
/// internally `Arc`-wrapped, so each caller receives a cheap clone.
static SHARED_DB: OnceCell<Result<DatabaseConnection, DatabaseInitError>> = OnceCell::const_new();

/// Settings applied to *every* connection in the pool.
///
/// All three are stated explicitly rather than inherited:
///
/// - `foreign_keys` is what makes `playlist_entries.local_track_id`'s
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

/// Open a connection pool against `db_path` and run every pending migration,
/// copying the database aside first when it already holds a library.
async fn connect_and_migrate(db_path: &Path) -> Result<DatabaseConnection, DatabaseInitError> {
    let pool = SqlitePoolOptions::new()
        .connect_with(sqlite_connect_options(db_path))
        .await
        .map_err(|e| {
            DatabaseInitError::new(
                DatabaseInitFailure::Open,
                format!("Failed to open database: {e}"),
            )
        })?;
    let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);

    let backup = match upgrade::ledger_state(&db).await {
        Ok(LedgerState::Newer { unknown }) => {
            let backups = upgrade::backup_dir(db_path);
            let detail = format!("a newer build applied migration {unknown}");
            return Err(DatabaseInitError::new(
                DatabaseInitFailure::NewerVersion { backups },
                detail,
            ));
        }
        Ok(LedgerState::Known { applied, pending }) if applied > 0 && pending > 0 => {
            back_up_before_upgrade(&db, db_path, applied).await
        }
        Ok(LedgerState::Known { .. }) => None,
        Err(error) => {
            return Err(DatabaseInitError::new(
                DatabaseInitFailure::Upgrade { backup: None },
                error.to_string(),
            ));
        }
    };

    info!("Running pending migrations");
    let upgrade = async {
        Migrator::up(&db, None).await?;
        migration::revalidate_critical_objects(&db).await
    };
    upgrade.await.map_err(|error| {
        DatabaseInitError::new(DatabaseInitFailure::Upgrade { backup }, error.to_string())
    })?;

    Ok(db)
}

/// A failed copy is logged and the upgrade goes ahead: refusing to start
/// would leave the library just as unavailable as a failed upgrade.
async fn back_up_before_upgrade(
    db: &DatabaseConnection,
    db_path: &Path,
    applied: usize,
) -> Option<PathBuf> {
    match upgrade::back_up(db, db_path, applied).await {
        Ok(path) => {
            info!(path = %path.display(), "Backed up the library database");
            Some(path)
        }
        Err(error) => {
            warn!(error = %format!("{error:#}"), "Upgrading the library database without a backup");
            None
        }
    }
}

/// Which start-up stage failed, so the UI can name it without showing the
/// underlying error text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabaseInitFailure {
    /// The data directory or the database file could not be opened.
    Open,
    /// A schema migration or the post-migration schema check failed.
    /// `backup` is the copy taken before the migrations started, if any.
    Upgrade { backup: Option<PathBuf> },
    /// A newer Tributary has migrated the database. Downgrades are not
    /// supported; `backups` is where the pre-upgrade copies are kept.
    NewerVersion { backups: PathBuf },
}

/// Why the shared database is unavailable for the rest of this process.
#[derive(Clone, Debug)]
pub struct DatabaseInitError {
    failure: DatabaseInitFailure,
    detail: String,
}

impl DatabaseInitError {
    fn new(failure: DatabaseInitFailure, detail: impl Into<String>) -> Self {
        Self {
            failure,
            detail: detail.into(),
        }
    }

    /// The failed stage, for user-facing copy.
    pub const fn failure(&self) -> &DatabaseInitFailure {
        &self.failure
    }
}

impl std::fmt::Display for DatabaseInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl From<DatabaseInitError> for DbErr {
    fn from(error: DatabaseInitError) -> Self {
        match error.failure {
            DatabaseInitFailure::Open => Self::Custom(error.detail),
            DatabaseInitFailure::Upgrade { .. } | DatabaseInitFailure::NewerVersion { .. } => {
                Self::Migration(error.detail)
            }
        }
    }
}

/// Obtain the shared database connection, initialising it on first call.
///
/// The first invocation opens the SQLite file, enables WAL mode, and
/// runs pending migrations. Every later call returns the same connection,
/// or the same failure, without touching the file again.
pub async fn get_or_init_db() -> Result<DatabaseConnection, DatabaseInitError> {
    SHARED_DB
        .get_or_init(|| async {
            // Return errors instead of panicking: callers wrap this in
            // graceful `match init_db() { Err(e) => … }` handling, and a
            // panic inside this spawned task would be swallowed by tokio,
            // silently killing the library engine with no user feedback.
            let data_dir = crate::paths::data_dir()
                .ok_or_else(|| {
                    DatabaseInitError::new(
                        DatabaseInitFailure::Open,
                        "Could not determine data directory",
                    )
                })?
                .join("tributary");

            std::fs::create_dir_all(&data_dir).map_err(|e| {
                DatabaseInitError::new(
                    DatabaseInitFailure::Open,
                    format!("Failed to create data directory: {e}"),
                )
            })?;

            let db_path = data_dir.join("library.db");
            info!(path = %db_path.display(), "Opening database");

            let db = connect_and_migrate(&db_path).await?;

            info!("Database ready");
            Ok(db)
        })
        .await
        .clone()
}

/// [`get_or_init_db`] with the failure as a [`DbErr`], for callers that fold
/// it into their own database error handling.
pub async fn init_db() -> Result<DatabaseConnection, DbErr> {
    get_or_init_db().await.map_err(DbErr::from)
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
            .query_one_raw(Statement::from_string(
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
            last_played_at_ms: Set(None),
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

    /// A playlist rename reads before it writes. While another pooled
    /// connection holds the write lock it must wait for that writer and then
    /// succeed, instead of failing with "database is locked" the moment its
    /// read transaction tries to become a writer.
    #[tokio::test]
    async fn read_then_write_playlist_mutation_waits_for_a_concurrent_writer() {
        let file = TestDatabase::new("write-lock");
        let db = connect_and_migrate(file.path())
            .await
            .expect("open database");
        let manager = crate::local::playlist_manager::PlaylistManager::new(db.clone());
        let playlist = manager
            .create_regular_playlist("Before")
            .await
            .expect("create playlist");

        let writer = crate::db::begin_write(&db)
            .await
            .expect("begin competing write");
        a_track("track-1", "/music/one.flac")
            .insert(&writer)
            .await
            .expect("stage competing write");

        let rename = tokio::spawn({
            let id = playlist.id.clone();
            async move { manager.rename_playlist(&id, "After").await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !rename.is_finished(),
            "the rename must still be waiting for the write lock"
        );
        writer.commit().await.expect("commit competing write");

        rename
            .await
            .expect("rename task")
            .expect("rename succeeds once the competing writer commits");
        let renamed = playlist::Entity::find_by_id(playlist.id)
            .one(&db)
            .await
            .expect("load playlist")
            .expect("playlist exists");
        assert_eq!(renamed.name, "After");
    }

    /// A current migration ledger is not proof that mutable critical SQLite
    /// objects still exist. Every process startup must validate them after
    /// the migrator's otherwise-no-op ledger check.
    #[tokio::test]
    async fn startup_revalidates_critical_objects_with_a_current_ledger() {
        let file = TestDatabase::new("critical-revalidation");
        let db = connect_and_migrate(file.path())
            .await
            .expect("open canonical database");
        db.execute_unprepared(
            "DROP TRIGGER trg_playlist_sidebar_revision_server_playlist_links_update",
        )
        .await
        .expect("simulate deleted critical trigger");
        db.close().await.expect("close first database pool");

        let error = connect_and_migrate(file.path())
            .await
            .expect_err("startup must reject a current but damaged migration installation");
        assert!(error.to_string().contains("trigger object"));
        assert_eq!(
            error.failure(),
            &DatabaseInitFailure::Upgrade { backup: None }
        );
    }

    #[tokio::test]
    async fn an_unopenable_database_is_an_open_failure() {
        let path = std::env::temp_dir()
            .join(format!("tributary-db-missing-{}", Uuid::new_v4()))
            .join("library.db");
        let error = connect_and_migrate(&path)
            .await
            .expect_err("the parent directory does not exist");
        assert_eq!(error.failure(), &DatabaseInitFailure::Open);
    }

    /// A library from an older release is copied aside once, at its old
    /// schema, and then upgraded through the whole chain on a file-backed
    /// multi-connection pool without losing its rows.
    #[tokio::test]
    async fn an_older_library_is_backed_up_and_then_upgraded() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("library.db");
        {
            let pool = SqlitePoolOptions::new()
                .connect_with(sqlite_connect_options(&path))
                .await
                .expect("open legacy database");
            let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
            Migrator::up(&db, Some(3))
                .await
                .expect("apply the early schema");
            db.execute_unprepared(
                "INSERT INTO tracks (id, file_path, title, artist_name, album_title, \
                 play_count, date_added, date_modified) VALUES ('track-1', \
                 '/music/one.flac', 'Title', 'Artist', 'Album', 7, \
                 '2025-01-01T00:00:00+00:00', '2025-01-01T00:00:00+00:00')",
            )
            .await
            .expect("insert legacy track");
            db.close().await.expect("close legacy database");
        }

        let db = connect_and_migrate(&path).await.expect("upgrade database");
        let upgraded = track::Entity::find_by_id("track-1".to_string())
            .one(&db)
            .await
            .expect("load track")
            .expect("track survives the upgrade");
        assert_eq!(upgraded.play_count, 7);
        db.close().await.expect("close upgraded database");

        let backups = backup_files(&dir.path().join("backups"));
        assert_eq!(backups.len(), 1, "{backups:?}");
        let name = backups[0].file_name().and_then(|name| name.to_str());
        assert!(
            name.is_some_and(|name| name.starts_with("library-schema3-")),
            "{name:?}"
        );
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&backups[0])
                    .read_only(true),
            )
            .await
            .expect("open backup");
        let copy = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
        let ledger = Migrator::get_migration_models(&copy)
            .await
            .expect("read backup ledger");
        assert_eq!(ledger.len(), 3, "the copy predates the upgrade");
        let row = copy
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT play_count FROM tracks WHERE id = 'track-1'",
            ))
            .await
            .expect("query backup")
            .expect("the copy holds the legacy track");
        assert_eq!(row.try_get::<i32>("", "play_count").expect("play count"), 7);

        connect_and_migrate(&path)
            .await
            .expect("reopen current database");
        assert_eq!(
            backup_files(&dir.path().join("backups")).len(),
            1,
            "a current database is not copied again"
        );
    }

    #[tokio::test]
    async fn a_new_database_is_not_backed_up() {
        let dir = tempfile::tempdir().expect("temporary directory");
        connect_and_migrate(&dir.path().join("library.db"))
            .await
            .expect("create database");
        assert!(!dir.path().join("backups").exists());
    }

    /// A downgrade meets a ledger naming migrations this build lacks. It is
    /// reported as such, before anything touches the database.
    #[tokio::test]
    async fn a_database_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("library.db");
        let db = connect_and_migrate(&path).await.expect("create database");
        db.execute_unprepared(
            "INSERT INTO seaql_migrations (version, applied_at) \
             VALUES ('m20991231_000099_from_a_newer_build', 0)",
        )
        .await
        .expect("record a future migration");
        db.close().await.expect("close database");

        let error = connect_and_migrate(&path)
            .await
            .expect_err("an older build must refuse a newer ledger");
        assert_eq!(
            error.failure(),
            &DatabaseInitFailure::NewerVersion {
                backups: dir.path().join("backups")
            }
        );
        assert!(!dir.path().join("backups").exists());
    }

    fn backup_files(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .map(|entry| entry.expect("backup entry").path())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// P1.5's guarantee, asserted end to end against a real pool: deleting a
    /// local track nulls only the current local binding. The entry and its
    /// durable source-scoped identity survive for unavailable presentation.
    #[tokio::test]
    async fn deleting_a_track_nulls_only_its_playlist_local_binding() {
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
            source_id: Set(crate::architecture::SourceId::local().to_string()),
            track_id: Set(Some("track-1".to_string())),
            local_track_id: Set(Some("track-1".to_string())),
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
        assert_eq!(entry.track_id.as_deref(), Some("track-1"));
        assert_eq!(entry.local_track_id, None);
        assert_eq!(
            entry.source_id,
            crate::architecture::SourceId::local().to_string()
        );
        assert_eq!(entry.position, 0);
        assert_eq!(entry.match_title, "Title");
    }
}
