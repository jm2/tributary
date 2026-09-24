//! Checks made before migrating: refuse a database that a newer build has
//! migrated, and copy the database aside before this build changes it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement};
use sea_orm_migration::MigratorTrait;

use super::migration::Migrator;

/// Folder next to `library.db` that holds the pre-upgrade copies.
const BACKUP_DIR: &str = "backups";
/// How many pre-upgrade copies are kept.
const BACKUPS_KEPT: usize = 3;
const BACKUP_PREFIX: &str = "library-";
const BACKUP_SUFFIX: &str = ".db";

/// What the migration ledger says about this build's migrations.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum LedgerState {
    /// Every recorded migration belongs to this build.
    Known { applied: usize, pending: usize },
    /// The ledger records a migration this build does not have, so a newer
    /// Tributary has migrated the database. sea-orm-migration refuses to run
    /// against such a ledger, and nothing here can undo a newer schema.
    Newer { unknown: String },
}

pub(super) async fn ledger_state(db: &DatabaseConnection) -> Result<LedgerState, DbErr> {
    let known: HashSet<String> = Migrator::migrations()
        .iter()
        .map(|migration| migration.name().to_string())
        .collect();
    let applied = Migrator::get_migration_models(db).await?;
    if let Some(model) = applied.iter().find(|model| !known.contains(&model.version)) {
        return Ok(LedgerState::Newer {
            unknown: model.version.clone(),
        });
    }
    Ok(LedgerState::Known {
        applied: applied.len(),
        pending: known.len().saturating_sub(applied.len()),
    })
}

/// The folder that holds pre-upgrade copies of `db_path`.
pub(super) fn backup_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(BACKUP_DIR)
}

/// Copy the database to `backups/library-schema<applied>-<UTC time>.db`
/// before migrations change it, keeping only the newest few copies.
///
/// If the newest copy was taken from the same schema, the upgrade that made
/// it did not finish. SQLite migrations are not transactional, so the live
/// file may now be partly migrated; that earlier copy is the one worth
/// keeping, and it is returned instead of taking another.
pub(super) async fn back_up(
    db: &DatabaseConnection,
    db_path: &Path,
    applied: usize,
) -> anyhow::Result<PathBuf> {
    let dir = backup_dir(db_path);
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let label = format!("schema{applied}");
    if let Some(newest) = existing_backups(&dir)?
        .pop()
        .filter(|backup| backup.label == label)
    {
        return Ok(newest.path);
    }

    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = dir.join(format!("{BACKUP_PREFIX}{label}-{stamp}{BACKUP_SUFFIX}"));
    let target = path
        .to_str()
        .context("the backup path is not valid UTF-8")?
        .to_string();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "VACUUM INTO ?",
        [target.into()],
    ))
    .await
    .context("VACUUM INTO failed")?;
    // The new copy exists either way; an old one left behind is harmless.
    if let Err(error) = prune(&dir) {
        tracing::warn!(error = %format!("{error:#}"), "Could not remove old library backups");
    }
    Ok(path)
}

struct Backup {
    path: PathBuf,
    label: String,
    stamp: String,
}

/// Backups in `dir`, oldest first. Files not named like a backup are
/// ignored, so pruning never touches anything else in the folder.
fn existing_backups(dir: &Path) -> anyhow::Result<Vec<Backup>> {
    let mut backups = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("could not read {}", dir.display()))?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((label, stamp)) = name
            .strip_prefix(BACKUP_PREFIX)
            .and_then(|rest| rest.strip_suffix(BACKUP_SUFFIX))
            .and_then(|rest| rest.rsplit_once('-'))
        else {
            continue;
        };
        if label.is_empty() || stamp.is_empty() || !path.is_file() {
            continue;
        }
        backups.push(Backup {
            label: label.to_string(),
            stamp: stamp.to_string(),
            path,
        });
    }
    backups.sort_by(|a, b| a.stamp.cmp(&b.stamp).then_with(|| a.path.cmp(&b.path)));
    Ok(backups)
}

fn prune(dir: &Path) -> anyhow::Result<()> {
    let backups = existing_backups(dir)?;
    let excess = backups.len().saturating_sub(BACKUPS_KEPT);
    for backup in &backups[..excess] {
        std::fs::remove_file(&backup.path)
            .with_context(|| format!("could not remove {}", backup.path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sea_orm::SqlxSqliteConnector;

    use super::*;

    /// A retried upgrade from the same schema reuses the copy the first
    /// attempt made, because the live file may since be partly migrated.
    #[tokio::test]
    async fn a_retried_upgrade_keeps_the_first_copy_of_that_schema() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let db_path = dir.path().join("library.db");
        // File-backed: an in-memory source would VACUUM INTO memory too.
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(true),
            )
            .await
            .expect("open database");
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
        Migrator::up(&db, Some(3))
            .await
            .expect("apply the early schema");

        let first = back_up(&db, &db_path, 3).await.expect("first backup");
        assert!(first.is_file());
        let retried = back_up(&db, &db_path, 3).await.expect("retried backup");
        assert_eq!(retried, first);

        let next = back_up(&db, &db_path, 4).await.expect("next schema backup");
        assert_ne!(next, first);
        assert_eq!(
            existing_backups(&backup_dir(&db_path)).expect("list").len(),
            2
        );
    }

    #[test]
    fn pruning_keeps_the_newest_backups_and_ignores_other_files() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let names = [
            "library-schema9-20260101T000000Z.db",
            "library-schema19-20260301T000000Z.db",
            "library-schema12-20260201T000000Z.db",
            "library-schema20-20260401T000000Z.db",
            "library-schema3-20250101T000000Z.db",
            "notes.txt",
            "library.db",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), b"x").expect("write fixture");
        }

        prune(dir.path()).expect("prune backups");

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list backups")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8")
            })
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "library-schema12-20260201T000000Z.db",
                "library-schema19-20260301T000000Z.db",
                "library-schema20-20260401T000000Z.db",
                "library.db",
                "notes.txt",
            ]
        );
    }
}
