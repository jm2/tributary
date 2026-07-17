//! Playback-time resolution for local-library identities.
//!
//! Queue and playlist rows keep the exact SQLite `tracks.id`; this module is
//! the only boundary that turns that identity back into a file URI. Metadata
//! or path matching is deliberately absent: those heuristics belong to
//! playlist reconciliation, never playback.

use std::path::Path;
use std::time::Duration;

use sea_orm::{DatabaseConnection, DbErr, EntityTrait};
use thiserror::Error;
use url::Url;

use crate::db::entities::track;

/// A dead or remote filesystem must not leave GTK waiting indefinitely for a
/// local load decision. SQLite has its own five-second busy timeout; use the
/// same outer budget for the point-in-time file probe.
const FILE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A closed, path-free local resolution failure safe for application logs.
#[derive(Debug, Error)]
pub enum LocalMediaResolutionError {
    #[error("local track identity is invalid")]
    InvalidTrackId,
    #[error("local track is no longer in the library")]
    Missing,
    #[error("local media database lookup failed")]
    Database {
        #[source]
        source: DbErr,
    },
    #[error("local media file is unavailable")]
    FileUnavailable {
        #[source]
        source: std::io::Error,
    },
    #[error("local media file check timed out")]
    FileCheckTimedOut,
    #[error("local media path cannot be represented as a file URI")]
    InvalidPath,
}

/// Resolve one exact source-native local track ID against the current database
/// row and filesystem immediately before an output load.
///
/// The lookup never falls back to a captured path, playlist fingerprint,
/// metadata, or another track. A row removed after queue creation is therefore
/// unavailable, while a committed rename is observed without rewriting the
/// queue. Root-authority leases remain a later P3.1 slice; this function makes
/// no claim that its point-in-time file check retains authority after return.
pub async fn resolve_track_uri(
    db: &DatabaseConnection,
    track_id: &str,
) -> Result<Url, LocalMediaResolutionError> {
    if track_id.is_empty() {
        return Err(LocalMediaResolutionError::InvalidTrackId);
    }

    let model = track::Entity::find_by_id(track_id.to_string())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::Missing)?;

    let path = Path::new(&model.file_path);
    let metadata = tokio::time::timeout(FILE_PROBE_TIMEOUT, tokio::fs::metadata(path))
        .await
        .map_err(|_| LocalMediaResolutionError::FileCheckTimedOut)?
        .map_err(|source| LocalMediaResolutionError::FileUnavailable { source })?;
    if !metadata.is_file() {
        return Err(LocalMediaResolutionError::FileUnavailable {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "resolved local media is not a regular file",
            ),
        });
    }

    Url::from_file_path(path).map_err(|()| LocalMediaResolutionError::InvalidPath)
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set};
    use sea_orm_migration::MigratorTrait;

    use super::*;
    use crate::db::migration::Migrator;

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open test database");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    fn model(id: &str, path: &Path) -> track::ActiveModel {
        track::ActiveModel {
            id: Set(id.to_string()),
            file_path: Set(path.to_string_lossy().into_owned()),
            title: Set("Title".to_string()),
            artist_name: Set("Artist".to_string()),
            album_title: Set("Album".to_string()),
            play_count: Set(0),
            date_added: Set("2026-07-17T00:00:00+00:00".to_string()),
            date_modified: Set("2026-07-17T00:00:00+00:00".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn exact_non_uuid_id_observes_a_committed_rename_at_use() {
        let db = database().await;
        let directory = tempfile::tempdir().expect("temporary media directory");
        let old_path = directory.path().join("old.flac");
        let new_path = directory.path().join("renamed.flac");
        std::fs::write(&old_path, b"old").expect("write old fixture");
        std::fs::write(&new_path, b"new").expect("write renamed fixture");

        model("legacy:not-a-uuid", &old_path)
            .insert(&db)
            .await
            .expect("insert exact legacy ID");

        let old_uri = resolve_track_uri(&db, "legacy:not-a-uuid")
            .await
            .expect("resolve original row");
        assert_eq!(
            old_uri.to_file_path().ok().as_deref(),
            Some(old_path.as_path())
        );

        let mut active: track::ActiveModel = track::Entity::find_by_id("legacy:not-a-uuid")
            .one(&db)
            .await
            .expect("query row")
            .expect("row exists")
            .into();
        active.file_path = Set(new_path.to_string_lossy().into_owned());
        active.update(&db).await.expect("commit rename");

        let current_uri = resolve_track_uri(&db, "legacy:not-a-uuid")
            .await
            .expect("resolve renamed row");
        assert_eq!(
            current_uri.to_file_path().ok().as_deref(),
            Some(new_path.as_path())
        );
    }

    #[tokio::test]
    async fn resolution_never_falls_back_for_invalid_missing_or_dead_ids() {
        let db = database().await;
        let directory = tempfile::tempdir().expect("temporary media directory");
        let dead_path = directory.path().join("gone.flac");
        model("dead-track", &dead_path)
            .insert(&db)
            .await
            .expect("insert dead track");

        assert!(matches!(
            resolve_track_uri(&db, "").await,
            Err(LocalMediaResolutionError::InvalidTrackId)
        ));
        assert!(matches!(
            resolve_track_uri(&db, "different-track").await,
            Err(LocalMediaResolutionError::Missing)
        ));
        let dead = resolve_track_uri(&db, "dead-track")
            .await
            .expect_err("dead path must fail");
        assert!(matches!(
            &dead,
            LocalMediaResolutionError::FileUnavailable { .. }
        ));
        let rendered = dead.to_string();
        assert_eq!(rendered, "local media file is unavailable");
        assert!(!rendered.contains("dead-track"));
        assert!(!rendered.contains("gone.flac"));
    }
}
