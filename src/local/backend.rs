//! `LocalBackend` — `MediaBackend` implementation for the local SQLite library.

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Statement};
use uuid::Uuid;

use crate::architecture::backend::{BackendResult, MediaBackend};
use crate::architecture::error::BackendError;
use crate::architecture::models::*;
use crate::architecture::TrackId;
use crate::db::entities::track;

use super::engine::db_model_to_track;

/// Private, versioned namespace for local aggregate identities.
///
/// Changing this value would invalidate every local album and artist reference,
/// so a future identity format must use a new namespace and an explicit migration.
const LOCAL_AGGREGATE_NAMESPACE_V1: Uuid = Uuid::from_u128(0x43eab0bf_1a52_52f0_a1fd_a2c17ec371d6);
const ARTIST_IDENTITY_DOMAIN: &[u8] = b"artist";
const ALBUM_IDENTITY_DOMAIN: &[u8] = b"album";

/// Local filesystem backend backed by SQLite.
pub struct LocalBackend {
    db: DatabaseConnection,
}

impl LocalBackend {
    /// Create a new local backend with the given database connection.
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

/// Return the album-artist grouping value while preserving every nonblank tag
/// byte-for-byte. A tag containing only Unicode whitespace is considered absent.
pub(super) fn effective_album_artist<'a>(
    album_artist_name: Option<&'a str>,
    artist_name: &'a str,
) -> &'a str {
    match album_artist_name {
        Some(album_artist_name) if !album_artist_name.trim().is_empty() => album_artist_name,
        _ => artist_name,
    }
}

fn append_identity_component(evidence: &mut Vec<u8>, component: &[u8]) {
    evidence.extend_from_slice(&(component.len() as u64).to_be_bytes());
    evidence.extend_from_slice(component);
}

fn local_aggregate_id(domain: &[u8], components: &[&str]) -> Uuid {
    let mut evidence = Vec::new();
    append_identity_component(&mut evidence, b"tributary-local-aggregate-v1");
    append_identity_component(&mut evidence, domain);
    evidence.extend_from_slice(&(components.len() as u64).to_be_bytes());
    for component in components {
        append_identity_component(&mut evidence, component.as_bytes());
    }
    Uuid::new_v5(&LOCAL_AGGREGATE_NAMESPACE_V1, &evidence)
}

pub(super) fn local_artist_id(artist_name: &str) -> Uuid {
    local_aggregate_id(ARTIST_IDENTITY_DOMAIN, &[artist_name])
}

pub(super) fn local_album_id(album_title: &str, effective_album_artist: &str) -> Uuid {
    local_aggregate_id(
        ALBUM_IDENTITY_DOMAIN,
        &[album_title, effective_album_artist],
    )
}

#[async_trait]
impl MediaBackend for LocalBackend {
    async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
        track::Entity::find()
            .all(&self.db)
            .await
            .map(|rows| rows.iter().map(db_model_to_track).collect())
            .map_err(|error| BackendError::Internal(error.into()))
    }

    fn rating_capability(&self) -> RatingCapability {
        RatingCapability::Writable
    }

    async fn set_track_rating(
        &self,
        track_id: &TrackId,
        rating: Option<Rating>,
    ) -> BackendResult<Option<Track>> {
        let transaction = crate::db::begin_write(&self.db)
            .await
            .map_err(|error| BackendError::Internal(error.into()))?;
        let update = transaction
            .execute_raw(Statement::from_sql_and_values(
                transaction.get_database_backend(),
                "UPDATE tracks SET rating = ? WHERE id = ?",
                [
                    rating.map(|value| i32::from(value.value())).into(),
                    track_id.as_str().into(),
                ],
            ))
            .await;
        let update = match update {
            Ok(update) => update,
            Err(error) => {
                let _ = transaction.rollback().await;
                return Err(BackendError::Internal(error.into()));
            }
        };

        if update.rows_affected() == 0 {
            transaction
                .commit()
                .await
                .map_err(|error| BackendError::Internal(error.into()))?;
            return Ok(None);
        }
        if update.rows_affected() != 1 {
            let affected = update.rows_affected();
            let _ = transaction.rollback().await;
            return Err(BackendError::Internal(anyhow::anyhow!(
                "rating update for exact track ID affected {affected} rows"
            )));
        }

        let updated = match track::Entity::find_by_id(track_id.as_str())
            .one(&transaction)
            .await
        {
            Ok(Some(updated)) => updated,
            Ok(None) => {
                let _ = transaction.rollback().await;
                return Err(BackendError::Internal(anyhow::anyhow!(
                    "rated track disappeared before commit"
                )));
            }
            Err(error) => {
                let _ = transaction.rollback().await;
                return Err(BackendError::Internal(error.into()));
            }
        };

        transaction
            .commit()
            .await
            .map_err(|error| BackendError::Internal(error.into()))?;
        Ok(Some(db_model_to_track(&updated)))
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
    use sea_orm_migration::MigratorTrait;

    use super::*;
    use crate::db::migration::Migrator;

    async fn in_memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite database");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    struct TrackFixture<'a> {
        id: u128,
        title: &'a str,
        artist: &'a str,
        album_artist: Option<&'a str>,
        album: &'a str,
        year: Option<i32>,
        genre: Option<&'a str>,
        duration_secs: Option<i64>,
        track_number: i32,
    }

    async fn insert_track(db: &DatabaseConnection, fixture: TrackFixture<'_>) {
        track::ActiveModel {
            id: Set(Uuid::from_u128(fixture.id).to_string()),
            file_path: Set(format!("/music/{}.flac", fixture.id)),
            title: Set(fixture.title.to_owned()),
            artist_name: Set(fixture.artist.to_owned()),
            album_artist_name: Set(fixture.album_artist.map(str::to_owned)),
            album_title: Set(fixture.album.to_owned()),
            genre: Set(fixture.genre.map(str::to_owned)),
            composer: Set(None),
            year: Set(fixture.year),
            track_number: Set(Some(fixture.track_number)),
            disc_number: Set(Some(1)),
            duration_secs: Set(fixture.duration_secs),
            bitrate_kbps: Set(None),
            sample_rate_hz: Set(None),
            format: Set(Some("FLAC".to_owned())),
            play_count: Set(0),
            last_played_at_ms: Set(None),
            rating: Set(None),
            date_added: Set("2026-07-17T00:00:00Z".to_owned()),
            date_modified: Set("2026-07-17T00:00:00Z".to_owned()),
            file_size_bytes: Set(None),
        }
        .insert(db)
        .await
        .expect("insert track fixture");
    }

    async fn populated_backend() -> LocalBackend {
        let db = in_memory_db().await;
        let fixtures = [
            TrackFixture {
                id: 1,
                title: "Compilation Second",
                artist: "Performer One",
                album_artist: Some("Compilation Artist"),
                album: "Shared Title",
                year: Some(2022),
                genre: Some("Rock"),
                duration_secs: Some(120),
                track_number: 2,
            },
            TrackFixture {
                id: 2,
                title: "Compilation First",
                artist: "Performer Two",
                album_artist: Some("Compilation Artist"),
                album: "Shared Title",
                year: Some(2020),
                genre: Some("Jazz"),
                duration_secs: None,
                track_number: 1,
            },
            TrackFixture {
                id: 3,
                title: "Performer's Album",
                artist: "Performer One",
                album_artist: None,
                album: "Shared Title",
                year: Some(2021),
                genre: Some("Pop"),
                duration_secs: Some(60),
                track_number: 1,
            },
            TrackFixture {
                id: 4,
                title: "Whitespace Album Artist",
                artist: "Whitespace Fallback",
                album_artist: Some("\u{2003}\t"),
                album: "Shared Title",
                year: None,
                genre: None,
                duration_secs: Some(30),
                track_number: 1,
            },
            TrackFixture {
                id: 5,
                title: "Other Edition",
                artist: "Performer One",
                album_artist: Some("Other Curator"),
                album: "Shared Title",
                year: Some(2024),
                genre: Some("Soul"),
                duration_secs: Some(20),
                track_number: 3,
            },
        ];
        for fixture in fixtures {
            insert_track(&db, fixture).await;
        }
        LocalBackend::new(db)
    }

    #[test]
    fn aggregate_ids_are_golden_domain_separated_and_collision_safe() {
        assert_eq!(
            local_artist_id("Exact Artist"),
            Uuid::parse_str("4813e9de-5abf-5720-bdd1-331e40d8c3fa").expect("artist golden UUID")
        );
        assert_eq!(
            local_album_id("Exact Album", "Exact Album Artist"),
            Uuid::parse_str("fd983813-0284-5736-8f0a-e81932b9aebe").expect("album golden UUID")
        );

        assert_ne!(local_artist_id("Same"), local_album_id("Same", ""));
        assert_ne!(local_album_id("ab", "c"), local_album_id("a", "bc"));
        assert_ne!(local_artist_id("Artist"), local_artist_id("artist"));
        assert_ne!(local_artist_id("Artist"), local_artist_id("Artist "));
    }

    #[test]
    fn effective_album_artist_only_falls_back_for_absent_or_blank_tags() {
        assert_eq!(effective_album_artist(None, "Performer"), "Performer");
        assert_eq!(
            effective_album_artist(Some(" \u{2003}\t"), "Performer"),
            "Performer"
        );
        assert_eq!(
            effective_album_artist(Some("  Album Artist  "), "Performer"),
            "  Album Artist  "
        );
    }

    #[tokio::test]
    async fn local_ratings_set_clear_and_target_only_the_exact_id() {
        let backend = populated_backend().await;
        assert_eq!(backend.rating_capability(), RatingCapability::Writable);
        let exact = TrackId::new(Uuid::from_u128(2).to_string()).expect("exact local ID");

        let rated = backend
            .set_track_rating(&exact, Some(Rating::new(73).unwrap()))
            .await
            .expect("persist rating")
            .expect("rated row exists");
        assert_eq!(
            rated.rating,
            TrackRating::writable(Some(Rating::new(73).unwrap()))
        );

        let sibling = track::Entity::find_by_id(Uuid::from_u128(1).to_string())
            .one(&backend.db)
            .await
            .expect("query sibling")
            .expect("sibling exists");
        assert_eq!(sibling.rating, None);

        let cleared = backend
            .set_track_rating(&exact, None)
            .await
            .expect("clear rating")
            .expect("cleared row exists");
        assert_eq!(cleared.rating, TrackRating::writable(None));
        let stored = track::Entity::find_by_id(exact.as_str())
            .one(&backend.db)
            .await
            .expect("query cleared row")
            .expect("cleared row exists");
        assert_eq!(stored.rating, None);

        let missing = TrackId::new("missing-local-track").unwrap();
        assert!(backend
            .set_track_rating(&missing, Some(Rating::new(50).unwrap()))
            .await
            .expect("missing rating write is a clean no-op")
            .is_none());

        backend
            .db
            .execute_unprepared(
                "INSERT INTO tracks (
                     id, file_path, title, artist_name, album_title, play_count,
                     date_added, date_modified
                 ) VALUES (
                     'legacy-non-uuid-id', '/music/legacy.flac', 'Legacy',
                     'Artist', 'Album', 0, 'added', 'modified'
                 )",
            )
            .await
            .expect("insert legacy non-UUID row");
        let legacy_id = TrackId::new("legacy-non-uuid-id").unwrap();
        let legacy = backend
            .set_track_rating(&legacy_id, Some(Rating::new(41).unwrap()))
            .await
            .expect("persist rating through exact legacy ID")
            .expect("legacy row exists");
        assert_eq!(legacy.native_track_id.as_ref(), Some(&legacy_id));
        assert_eq!(
            legacy.rating,
            TrackRating::writable(Some(Rating::new(41).unwrap()))
        );
    }

    #[tokio::test]
    async fn local_rating_update_rolls_back_if_row_disappears_before_fetch() {
        let backend = populated_backend().await;
        let exact_text = Uuid::from_u128(1).to_string();
        backend
            .db
            .execute_raw(Statement::from_sql_and_values(
                backend.db.get_database_backend(),
                "UPDATE tracks SET rating = ? WHERE id = ?",
                [35_i32.into(), exact_text.clone().into()],
            ))
            .await
            .expect("seed pre-update rating");
        backend
            .db
            .execute_unprepared(&format!(
                "CREATE TRIGGER delete_rated_track AFTER UPDATE OF rating ON tracks \
                 WHEN NEW.id = '{exact_text}' BEGIN \
                 DELETE FROM tracks WHERE id = NEW.id; END"
            ))
            .await
            .expect("install disappearance trigger");

        let exact = TrackId::new(exact_text.clone()).unwrap();
        let error = backend
            .set_track_rating(&exact, Some(Rating::new(92).unwrap()))
            .await
            .expect_err("post-update disappearance must fail closed");
        assert!(error.to_string().contains("disappeared before commit"));

        let restored = track::Entity::find_by_id(exact_text)
            .one(&backend.db)
            .await
            .expect("query rolled-back row")
            .expect("rollback restores deleted row");
        assert_eq!(restored.rating, Some(35));
    }
}
