//! Playback-time resolution for local-library identities.
//!
//! Queue and playlist rows keep the exact SQLite `tracks.id`; this module is
//! the only boundary that turns that identity into retained filesystem
//! authority. Metadata or path matching is deliberately absent: those
//! heuristics belong to playlist reconciliation, never playback.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::{DatabaseConnection, DbErr, EntityTrait};
use thiserror::Error;
use url::Url;

use crate::db::entities::{library_root, track};

use super::root_authority::{BoundFile, RootAuthorityLease};

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
    #[error("local track is outside the current configured library roots")]
    NoConfiguredRoot,
    #[error("local library root is not currently authoritative")]
    RootUnavailable,
    #[error("local media authority is unavailable")]
    AuthorityUnavailable {
        #[source]
        source: std::io::Error,
    },
    #[error("local media authority check timed out")]
    AuthorityCheckTimedOut,
    #[error("local track or root changed while media authority was acquired")]
    ChangedDuringResolution,
    #[error("local media path cannot be represented as a file URI")]
    InvalidPath,
}

struct ResolvedLocalMediaInner {
    authority: Arc<RootAuthorityLease>,
    file: BoundFile,
    path: PathBuf,
    file_uri: Url,
    extension: Option<String>,
}

/// Security-relevant database state captured before filesystem authority is
/// acquired.
///
/// `last_checked_at` is observational scan metadata, not an authority
/// generation: a concurrent successful scan may refresh it without changing
/// which root is trusted. Keep the comparison explicit so such timestamp-only
/// drift cannot spuriously reject an otherwise current resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedRootAuthorityState {
    path: String,
    device_id: Option<String>,
    identity_confirmed: bool,
    is_available: bool,
    last_scan_complete: bool,
}

impl ExpectedRootAuthorityState {
    fn from_model(state: &library_root::Model) -> Self {
        Self {
            path: state.path.clone(),
            device_id: state.device_id.clone(),
            identity_confirmed: state.identity_confirmed,
            is_available: state.is_available,
            last_scan_complete: state.last_scan_complete,
        }
    }

    fn matches(&self, state: &library_root::Model) -> bool {
        self.path == state.path
            && self.device_id == state.device_id
            && self.identity_confirmed == state.identity_confirmed
            && self.is_available == state.is_available
            && self.last_scan_complete == state.last_scan_complete
    }
}

/// Exact local file authority retained for one output load.
///
/// Clones share the same root, marker, ancestor, and file handles. Outputs and
/// their receiver-facing ticket servers keep a clone until replacement, Stop,
/// completion, failure, or teardown. No consumer reopens the database path.
#[derive(Clone)]
pub struct ResolvedLocalMedia {
    inner: Arc<ResolvedLocalMediaInner>,
}

impl std::fmt::Debug for ResolvedLocalMedia {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedLocalMedia")
            .finish_non_exhaustive()
    }
}

impl ResolvedLocalMedia {
    /// Clone the already-authorized file object for a new local consumer.
    ///
    /// Platform clone operations may share a cursor with the retained handle;
    /// streaming consumers must use position-independent reads.
    pub(crate) fn try_clone_file(&self) -> std::io::Result<File> {
        self.inner
            .file
            .try_clone_for_consumption(&self.inner.authority)
    }

    /// File URI for application-owned display/artwork helpers only.
    ///
    /// Playback outputs receive the owned lease through `load_local`, never
    /// this locator.
    pub(crate) fn file_uri(&self) -> &Url {
        &self.inner.file_uri
    }

    /// Safe extension hint used only to label an opaque media ticket.
    pub(crate) fn extension(&self) -> Option<&str> {
        self.inner.extension.as_deref()
    }

    /// Confirm that this lease's root is still the most-specific configured
    /// root for the resolved path. The caller performs this path-only check on
    /// the GTK thread immediately before handing the lease to an output.
    pub(crate) fn matches_current_configuration(&self, configured_roots: &[String]) -> bool {
        configured_roots
            .iter()
            .map(PathBuf::from)
            .filter(|root| root.is_absolute() && self.inner.path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .is_some_and(|root| root == self.inner.authority.root())
    }

    /// Construct retained local authority without a database for focused
    /// output-boundary tests.
    #[cfg(test)]
    pub(crate) fn from_authorized_path_for_test(
        root: &std::path::Path,
        expected_marker: &str,
        path: &std::path::Path,
    ) -> std::io::Result<Self> {
        let authority = Arc::new(RootAuthorityLease::acquire(root, expected_marker)?);
        let file = authority.open_regular_file(path)?;
        let file_uri = Url::from_file_path(path).map_err(|()| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "test media path cannot be represented as a file URI",
            )
        })?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_owned);
        Ok(Self {
            inner: Arc::new(ResolvedLocalMediaInner {
                authority,
                file,
                path: path.to_path_buf(),
                file_uri,
                extension,
            }),
        })
    }
}

fn configured_root_states<'a>(
    states: &'a [library_root::Model],
    configured_roots: &[String],
) -> Vec<(&'a library_root::Model, PathBuf)> {
    let configured: Vec<PathBuf> = configured_roots
        .iter()
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .collect();
    let mut matching: Vec<_> = states
        .iter()
        .filter_map(|state| {
            let root = PathBuf::from(&state.path);
            configured
                .iter()
                .any(|configured| configured == &root)
                .then_some((state, root))
        })
        .collect();
    matching.sort_by_key(|(_, root)| std::cmp::Reverse(root.components().count()));
    matching
}

/// Resolve one exact source-native local track ID against the current database
/// row and filesystem immediately before an output load.
///
/// The lookup never falls back to a captured path, playlist fingerprint,
/// metadata, or another track. A row removed after queue creation is therefore
/// unavailable, while a committed rename is observed without rewriting the
/// queue. The returned value retains root, marker, ancestor, and exact file
/// handles for the complete output/ticket lifecycle.
pub async fn resolve_track(
    db: &DatabaseConnection,
    track_id: &str,
    configured_roots: &[String],
) -> Result<ResolvedLocalMedia, LocalMediaResolutionError> {
    if track_id.is_empty() {
        return Err(LocalMediaResolutionError::InvalidTrackId);
    }

    let model = track::Entity::find_by_id(track_id.to_string())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::Missing)?;

    let states = library_root::Entity::find()
        .all(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?;
    let path = PathBuf::from(&model.file_path);
    let Some((state, root)) = configured_root_states(&states, configured_roots)
        .into_iter()
        .find(|(_, root)| path.starts_with(root))
    else {
        return Err(LocalMediaResolutionError::NoConfiguredRoot);
    };
    if !state.identity_confirmed || !state.is_available || !state.last_scan_complete {
        return Err(LocalMediaResolutionError::RootUnavailable);
    }
    let expected_marker = state
        .device_id
        .clone()
        .ok_or(LocalMediaResolutionError::RootUnavailable)?;
    let expected_root_state = ExpectedRootAuthorityState::from_model(state);
    let authority_path = path.clone();
    let authority_root = root.clone();
    let acquired = tokio::time::timeout(
        FILE_PROBE_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let authority = Arc::new(RootAuthorityLease::acquire(
                &authority_root,
                &expected_marker,
            )?);
            let file = authority.open_regular_file(&authority_path)?;
            Ok::<_, std::io::Error>((authority, file))
        }),
    )
    .await
    .map_err(|_| LocalMediaResolutionError::AuthorityCheckTimedOut)?
    .map_err(|source| LocalMediaResolutionError::AuthorityUnavailable {
        source: std::io::Error::other(format!("local authority task failed: {source}")),
    })?
    .map_err(|source| LocalMediaResolutionError::AuthorityUnavailable { source })?;

    // The blocking handle acquisition is intentionally outside SQLite. Re-read
    // both bindings afterward so a concurrent reconciliation/root demotion
    // cannot publish authority acquired for an obsolete database snapshot.
    let current_model = track::Entity::find_by_id(track_id.to_string())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::Missing)?;
    let current_state = library_root::Entity::find_by_id(expected_root_state.path.clone())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::ChangedDuringResolution)?;
    if current_model.file_path != model.file_path || !expected_root_state.matches(&current_state) {
        return Err(LocalMediaResolutionError::ChangedDuringResolution);
    }

    let file_uri =
        Url::from_file_path(&path).map_err(|()| LocalMediaResolutionError::InvalidPath)?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_owned);

    Ok(ResolvedLocalMedia {
        inner: Arc::new(ResolvedLocalMediaInner {
            authority: acquired.0,
            file: acquired.1,
            path,
            file_uri,
            extension,
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use std::path::Path;

    use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

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

    async fn authorize_root(db: &DatabaseConnection, root: &Path) -> String {
        let marker = format!("marker:v1:{}", Uuid::new_v4());
        std::fs::write(root.join(".tributary-root-id"), format!("{marker}\n"))
            .expect("write root marker");
        library_root::ActiveModel {
            path: Set(root.to_string_lossy().into_owned()),
            device_id: Set(Some(marker.clone())),
            identity_confirmed: Set(true),
            is_available: Set(true),
            last_scan_complete: Set(true),
            last_checked_at: Set("2026-07-17T00:00:00+00:00".to_string()),
        }
        .insert(db)
        .await
        .expect("insert authoritative root");
        marker
    }

    fn configured(root: &Path) -> Vec<String> {
        vec![root.to_string_lossy().into_owned()]
    }

    fn read_media(media: &ResolvedLocalMedia) -> Vec<u8> {
        let mut file = media.try_clone_file().expect("clone authorized file");
        file.seek(SeekFrom::Start(0))
            .expect("reset shared test cursor");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read authorized file");
        bytes
    }

    #[test]
    fn root_authority_snapshot_ignores_timestamp_but_binds_every_authority_field() {
        let original = library_root::Model {
            path: "/music".to_string(),
            device_id: Some("marker:v1:00000000-0000-4000-8000-000000000000".to_string()),
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-17T00:00:00Z".to_string(),
        };
        let expected = ExpectedRootAuthorityState::from_model(&original);

        let mut timestamp_only = original.clone();
        timestamp_only.last_checked_at = "2099-01-01T00:00:00Z".to_string();
        assert!(expected.matches(&timestamp_only));

        let mut changed = original.clone();
        changed.path = "/replacement".to_string();
        assert!(!expected.matches(&changed));

        let mut changed = original.clone();
        changed.device_id = Some("marker:v1:ffffffff-ffff-4fff-bfff-ffffffffffff".to_string());
        assert!(!expected.matches(&changed));

        let mut changed = original.clone();
        changed.identity_confirmed = false;
        assert!(!expected.matches(&changed));

        let mut changed = original.clone();
        changed.is_available = false;
        assert!(!expected.matches(&changed));

        let mut changed = original;
        changed.last_scan_complete = false;
        assert!(!expected.matches(&changed));
    }

    #[tokio::test]
    async fn exact_non_uuid_id_observes_a_committed_rename_at_use() {
        let db = database().await;
        let directory = tempfile::tempdir().expect("temporary media directory");
        let old_path = directory.path().join("old.flac");
        let new_path = directory.path().join("renamed.flac");
        std::fs::write(&old_path, b"old").expect("write old fixture");
        std::fs::write(&new_path, b"new").expect("write renamed fixture");
        authorize_root(&db, directory.path()).await;
        let roots = configured(directory.path());

        model("legacy:not-a-uuid", &old_path)
            .insert(&db)
            .await
            .expect("insert exact legacy ID");

        let old_media = resolve_track(&db, "legacy:not-a-uuid", &roots)
            .await
            .expect("resolve original row");
        assert_eq!(
            old_media.file_uri().to_file_path().ok().as_deref(),
            Some(old_path.as_path())
        );
        assert_eq!(read_media(&old_media), b"old");

        let mut active: track::ActiveModel = track::Entity::find_by_id("legacy:not-a-uuid")
            .one(&db)
            .await
            .expect("query row")
            .expect("row exists")
            .into();
        active.file_path = Set(new_path.to_string_lossy().into_owned());
        active.update(&db).await.expect("commit rename");

        let current_media = resolve_track(&db, "legacy:not-a-uuid", &roots)
            .await
            .expect("resolve renamed row");
        assert_eq!(
            current_media.file_uri().to_file_path().ok().as_deref(),
            Some(new_path.as_path())
        );
        assert_eq!(read_media(&current_media), b"new");
    }

    #[tokio::test]
    async fn resolution_never_falls_back_for_invalid_missing_or_dead_ids() {
        let db = database().await;
        let directory = tempfile::tempdir().expect("temporary media directory");
        let dead_path = directory.path().join("gone.flac");
        authorize_root(&db, directory.path()).await;
        let roots = configured(directory.path());
        model("dead-track", &dead_path)
            .insert(&db)
            .await
            .expect("insert dead track");

        assert!(matches!(
            resolve_track(&db, "", &roots).await,
            Err(LocalMediaResolutionError::InvalidTrackId)
        ));
        assert!(matches!(
            resolve_track(&db, "different-track", &roots).await,
            Err(LocalMediaResolutionError::Missing)
        ));
        let dead = resolve_track(&db, "dead-track", &roots)
            .await
            .expect_err("dead path must fail");
        assert!(matches!(
            &dead,
            LocalMediaResolutionError::AuthorityUnavailable { .. }
        ));
        let rendered = dead.to_string();
        assert_eq!(rendered, "local media authority is unavailable");
        assert!(!rendered.contains("dead-track"));
        assert!(!rendered.contains("gone.flac"));
    }

    #[tokio::test]
    async fn current_config_and_most_specific_root_fail_closed() {
        let db = database().await;
        let parent = tempfile::tempdir().expect("parent root");
        let child = parent.path().join("child");
        std::fs::create_dir(&child).expect("create child root");
        authorize_root(&db, parent.path()).await;
        authorize_root(&db, &child).await;
        let path = child.join("track.flac");
        std::fs::write(&path, b"track").expect("write track");
        model("track", &path)
            .insert(&db)
            .await
            .expect("insert track");

        assert!(matches!(
            resolve_track(&db, "track", &[]).await,
            Err(LocalMediaResolutionError::NoConfiguredRoot)
        ));

        let mut child_state: library_root::ActiveModel =
            library_root::Entity::find_by_id(child.to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query child state")
                .expect("child state exists")
                .into();
        child_state.is_available = Set(false);
        child_state.update(&db).await.expect("demote child root");
        let both = vec![
            parent.path().to_string_lossy().into_owned(),
            child.to_string_lossy().into_owned(),
        ];
        assert!(matches!(
            resolve_track(&db, "track", &both).await,
            Err(LocalMediaResolutionError::RootUnavailable)
        ));

        let parent_only = configured(parent.path());
        let media = resolve_track(&db, "track", &parent_only)
            .await
            .expect("the explicitly configured parent remains authoritative");
        assert!(media.matches_current_configuration(&parent_only));
        assert!(!media.matches_current_configuration(&configured(&child)));
        assert!(
            !media.matches_current_configuration(&both),
            "adding a more-specific configured root invalidates a parent-root result"
        );
    }

    #[tokio::test]
    async fn admitted_media_reads_the_retained_file_not_a_path_replacement() {
        let db = database().await;
        let root = tempfile::tempdir().expect("library root");
        authorize_root(&db, root.path()).await;
        let roots = configured(root.path());
        let path = root.path().join("track.flac");
        let displaced = root.path().join("displaced.flac");
        std::fs::write(&path, b"authorized").expect("write authorized file");
        model("track", &path)
            .insert(&db)
            .await
            .expect("insert track");

        let media = resolve_track(&db, "track", &roots)
            .await
            .expect("resolve exact file");
        let replacement_installed = match std::fs::rename(&path, &displaced) {
            Ok(()) => {
                std::fs::write(&path, b"replacement").expect("install path replacement");
                true
            }
            Err(error) => {
                #[cfg(not(windows))]
                panic!("move admitted file: {error}");
                #[cfg(windows)]
                {
                    let _ = error;
                    // Windows authority handles intentionally omit delete
                    // sharing. A blocked rename is the platform's stronger
                    // form of the same guarantee: the admitted name cannot be
                    // retargeted while held.
                    assert_eq!(
                        std::fs::read(&path).expect("read pinned path"),
                        b"authorized"
                    );
                    false
                }
            }
        };

        assert_eq!(read_media(&media), b"authorized");
        if replacement_installed {
            assert_ne!(
                read_media(&media),
                std::fs::read(&path).expect("read replacement")
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_never_authorized_by_a_textual_root_prefix() {
        use std::os::unix::fs::symlink;

        let db = database().await;
        let root = tempfile::tempdir().expect("library root");
        let outside = tempfile::tempdir().expect("outside directory");
        authorize_root(&db, root.path()).await;
        let outside_file = outside.path().join("outside.flac");
        std::fs::write(&outside_file, b"outside").expect("write outside file");
        let link = root.path().join("escape");
        symlink(outside.path(), &link).expect("create directory symlink");
        let escaped_path = link.join("outside.flac");
        model("escaped", &escaped_path)
            .insert(&db)
            .await
            .expect("insert escaped path");

        assert!(matches!(
            resolve_track(&db, "escaped", &configured(root.path())).await,
            Err(LocalMediaResolutionError::AuthorityUnavailable { .. })
        ));
    }
}
