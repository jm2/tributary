//! `LocalBackend` — `MediaBackend` implementation for the local SQLite library.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, Func};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
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

    /// Pre-aggregate rows that share all metadata needed to construct album
    /// and artist identities. The final effective-album-artist fold stays in
    /// Rust so it uses exactly the same Unicode-whitespace rule as track
    /// conversion without materialising every track column.
    async fn aggregate_fragments(&self) -> BackendResult<Vec<AggregateFragment>> {
        track::Entity::find()
            .select_only()
            .column(track::Column::AlbumTitle)
            .column(track::Column::ArtistName)
            .column(track::Column::AlbumArtistName)
            .column_as(track::Column::Year.min(), "year")
            .column_as(track::Column::Genre.min(), "genre")
            .column_as(track::Column::Id.count(), "track_count")
            .column_as(track::Column::DurationSecs.sum(), "total_duration_secs")
            .group_by(track::Column::AlbumTitle)
            .group_by(track::Column::ArtistName)
            .group_by(track::Column::AlbumArtistName)
            .into_model::<AggregateFragment>()
            .all(&self.db)
            .await
            .map_err(|error| BackendError::Internal(error.into()))
    }

    async fn resolve_album_key(&self, album_id: &Uuid) -> BackendResult<Option<(String, String)>> {
        let keys: BTreeSet<(String, String)> = self
            .aggregate_fragments()
            .await?
            .into_iter()
            .map(|row| {
                let effective_artist =
                    effective_album_artist(row.album_artist_name.as_deref(), &row.artist_name)
                        .to_owned();
                (row.album_title, effective_artist)
            })
            .collect();
        Ok(keys
            .into_iter()
            .find(|(title, artist)| local_album_id(title, artist) == *album_id))
    }

    async fn resolve_artist_name(&self, artist_id: &Uuid) -> BackendResult<Option<String>> {
        let names: BTreeSet<String> = self
            .aggregate_fragments()
            .await?
            .into_iter()
            .map(|row| row.artist_name)
            .collect();
        Ok(names
            .into_iter()
            .find(|name| local_artist_id(name) == *artist_id))
    }
}

/// One pre-aggregated metadata fragment. Several fragments can belong to one
/// logical album when their performing artists differ but their album artist
/// is the same.
#[derive(FromQueryResult)]
struct AggregateFragment {
    album_title: String,
    artist_name: String,
    album_artist_name: Option<String>,
    year: Option<i32>,
    genre: Option<String>,
    track_count: i64,
    total_duration_secs: Option<i64>,
}

#[derive(Default)]
struct AlbumTotals {
    year: Option<i32>,
    genre: Option<String>,
    track_count: i64,
    total_duration_secs: i64,
}

#[derive(Default)]
struct ArtistTotals {
    track_count: i64,
    album_keys: BTreeSet<(String, String)>,
}

/// The single row produced by the `get_stats` aggregate query.
#[derive(FromQueryResult, Default)]
struct StatsAgg {
    total_tracks: i64,
    total_duration_secs: Option<i64>,
    total_artists: i64,
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

fn minimum<T: Ord>(left: Option<T>, right: Option<T>) -> Option<T> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left @ Some(_), None) => left,
        (None, right) => right,
    }
}

#[async_trait]
impl MediaBackend for LocalBackend {
    fn name(&self) -> &str {
        "Local Filesystem"
    }

    fn backend_type(&self) -> &str {
        "local"
    }

    async fn ping(&self) -> BackendResult<()> {
        // Simple connectivity check: try to count tracks
        track::Entity::find()
            .one(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| BackendError::ConnectionFailed {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        let tracks: Vec<Track> = track::Entity::find()
            .filter(
                Condition::any()
                    .add(track::Column::Title.contains(query))
                    .add(track::Column::ArtistName.contains(query))
                    .add(track::Column::AlbumTitle.contains(query)),
            )
            // Bound the result set in SQL rather than materialising every
            // matching row and truncating in Rust.
            .limit(limit as u64)
            .all(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?
            .iter()
            .map(db_model_to_track)
            .collect();

        Ok(SearchResults {
            tracks,
            albums: vec![],
            artists: vec![],
        })
    }

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
        let transaction = self
            .db
            .begin()
            .await
            .map_err(|error| BackendError::Internal(error.into()))?;
        let update = transaction
            .execute(Statement::from_sql_and_values(
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

    async fn list_albums(&self, sort: SortField, order: SortOrder) -> BackendResult<Vec<Album>> {
        let mut grouped = BTreeMap::<(String, String), AlbumTotals>::new();
        for row in self.aggregate_fragments().await? {
            let effective_artist =
                effective_album_artist(row.album_artist_name.as_deref(), &row.artist_name)
                    .to_owned();
            let totals = grouped
                .entry((row.album_title, effective_artist))
                .or_default();
            totals.year = minimum(totals.year.take(), row.year);
            totals.genre = minimum(totals.genre.take(), row.genre);
            totals.track_count += row.track_count;
            totals.total_duration_secs += row.total_duration_secs.unwrap_or(0);
        }

        let mut albums: Vec<Album> = grouped
            .into_iter()
            .map(|((title, artist_name), totals)| Album {
                id: local_album_id(&title, &artist_name),
                title,
                artist_name,
                artist_id: None,
                year: totals.year,
                genre: totals.genre,
                cover_art_url: None,
                track_count: totals.track_count as u32,
                total_duration_secs: Some(totals.total_duration_secs as u64),
            })
            .collect();

        // Sort
        match sort {
            SortField::Title => albums.sort_by(|a, b| a.title.cmp(&b.title)),
            SortField::Artist => albums.sort_by(|a, b| a.artist_name.cmp(&b.artist_name)),
            SortField::Year => albums.sort_by_key(|a| a.year),
            _ => albums.sort_by(|a, b| a.title.cmp(&b.title)),
        }

        if matches!(order, SortOrder::Descending) {
            albums.reverse();
        }

        Ok(albums)
    }

    async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
        let mut grouped = BTreeMap::<String, ArtistTotals>::new();
        for row in self.aggregate_fragments().await? {
            let effective_artist =
                effective_album_artist(row.album_artist_name.as_deref(), &row.artist_name)
                    .to_owned();
            let totals = grouped.entry(row.artist_name).or_default();
            totals.track_count += row.track_count;
            totals
                .album_keys
                .insert((row.album_title, effective_artist));
        }

        Ok(grouped
            .into_iter()
            .map(|(name, totals)| Artist {
                id: local_artist_id(&name),
                name,
                album_count: totals.album_keys.len() as u32,
                track_count: totals.track_count as u32,
                cover_art_url: None,
            })
            .collect())
    }

    async fn get_album_tracks(&self, album_id: &Uuid) -> BackendResult<Vec<Track>> {
        let Some((album_title, effective_artist)) = self.resolve_album_key(album_id).await? else {
            return Ok(Vec::new());
        };
        let rows = track::Entity::find()
            .filter(track::Column::AlbumTitle.eq(album_title))
            .order_by_asc(track::Column::DiscNumber)
            .order_by_asc(track::Column::TrackNumber)
            .order_by_asc(track::Column::Title)
            .order_by_asc(track::Column::Id)
            .all(&self.db)
            .await
            .map_err(|error| BackendError::Internal(error.into()))?;

        Ok(rows
            .iter()
            .filter(|row| {
                let artist =
                    effective_album_artist(row.album_artist_name.as_deref(), &row.artist_name);
                artist == effective_artist
            })
            .map(db_model_to_track)
            .collect())
    }

    async fn get_artist_tracks(&self, artist_id: &Uuid) -> BackendResult<Vec<Track>> {
        let Some(artist_name) = self.resolve_artist_name(artist_id).await? else {
            return Ok(Vec::new());
        };
        let rows = track::Entity::find()
            .filter(track::Column::ArtistName.eq(artist_name))
            .order_by_asc(track::Column::AlbumTitle)
            .order_by_asc(track::Column::DiscNumber)
            .order_by_asc(track::Column::TrackNumber)
            .order_by_asc(track::Column::Title)
            .order_by_asc(track::Column::Id)
            .all(&self.db)
            .await
            .map_err(|error| BackendError::Internal(error.into()))?;

        Ok(rows.iter().map(db_model_to_track).collect())
    }

    async fn get_stats(&self) -> BackendResult<LibraryStats> {
        let stats = track::Entity::find()
            .select_only()
            .column_as(track::Column::Id.count(), "total_tracks")
            .column_as(track::Column::DurationSecs.sum(), "total_duration_secs")
            .column_as(
                Expr::expr(Func::count_distinct(Expr::col(track::Column::ArtistName))),
                "total_artists",
            )
            .into_model::<StatsAgg>()
            .one(&self.db)
            .await
            .map_err(|e| BackendError::Internal(e.into()))?
            .unwrap_or_default();

        let album_keys: BTreeSet<(String, String)> = self
            .aggregate_fragments()
            .await?
            .into_iter()
            .map(|row| {
                let effective_artist =
                    effective_album_artist(row.album_artist_name.as_deref(), &row.artist_name)
                        .to_owned();
                (row.album_title, effective_artist)
            })
            .collect();

        Ok(LibraryStats {
            total_tracks: stats.total_tracks as u64,
            total_albums: album_keys.len() as u64,
            total_artists: stats.total_artists as u64,
            // SUM is NULL on an empty table; the previous fold summed to 0.
            total_duration_secs: stats.total_duration_secs.unwrap_or(0) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
    async fn aggregates_are_stable_disambiguated_and_queryable() {
        let backend = populated_backend().await;

        let albums = backend
            .list_albums(SortField::Title, SortOrder::Ascending)
            .await
            .expect("list albums");
        let albums_again = backend
            .list_albums(SortField::Title, SortOrder::Ascending)
            .await
            .expect("list albums again");
        assert_eq!(albums.len(), 4);
        assert_eq!(
            albums.iter().map(|album| album.id).collect::<Vec<_>>(),
            albums_again
                .iter()
                .map(|album| album.id)
                .collect::<Vec<_>>()
        );
        assert!(albums.iter().all(|album| album.artist_id.is_none()));

        let compilation = albums
            .iter()
            .find(|album| album.artist_name == "Compilation Artist")
            .expect("compilation album");
        assert_eq!(
            compilation.id,
            local_album_id("Shared Title", "Compilation Artist")
        );
        assert_eq!(compilation.track_count, 2);
        assert_eq!(compilation.total_duration_secs, Some(120));
        assert_eq!(compilation.year, Some(2020));
        assert_eq!(compilation.genre.as_deref(), Some("Jazz"));

        let album_artists: BTreeSet<_> = albums
            .iter()
            .map(|album| album.artist_name.as_str())
            .collect();
        assert_eq!(
            album_artists,
            BTreeSet::from([
                "Compilation Artist",
                "Other Curator",
                "Performer One",
                "Whitespace Fallback",
            ])
        );

        let compilation_tracks = backend
            .get_album_tracks(&compilation.id)
            .await
            .expect("get compilation tracks");
        assert_eq!(
            compilation_tracks
                .iter()
                .map(|track| track.title.as_str())
                .collect::<Vec<_>>(),
            ["Compilation First", "Compilation Second"]
        );
        assert!(compilation_tracks
            .iter()
            .all(|track| track.album_id == Some(compilation.id)));
        assert_eq!(
            compilation_tracks[0].artist_id,
            Some(local_artist_id("Performer Two"))
        );

        let whitespace_album = albums
            .iter()
            .find(|album| album.artist_name == "Whitespace Fallback")
            .expect("whitespace-fallback album");
        let whitespace_tracks = backend
            .get_album_tracks(&whitespace_album.id)
            .await
            .expect("get whitespace-fallback album tracks");
        assert_eq!(
            whitespace_tracks[0].album_artist_name.as_deref(),
            Some("\u{2003}\t")
        );

        let artists = backend.list_artists().await.expect("list artists");
        let performer_one = artists
            .iter()
            .find(|artist| artist.name == "Performer One")
            .expect("performer one");
        assert_eq!(performer_one.id, local_artist_id("Performer One"));
        assert_eq!(performer_one.track_count, 3);
        assert_eq!(performer_one.album_count, 3);

        let performer_tracks = backend
            .get_artist_tracks(&performer_one.id)
            .await
            .expect("get performer tracks");
        assert_eq!(performer_tracks.len(), 3);
        assert!(performer_tracks
            .iter()
            .all(|track| track.artist_id == Some(performer_one.id)));

        let stats = backend.get_stats().await.expect("get stats");
        assert_eq!(stats.total_tracks, 5);
        assert_eq!(stats.total_albums, 4);
        assert_eq!(stats.total_artists, 3);
        assert_eq!(stats.total_duration_secs, 230);

        assert!(backend
            .get_album_tracks(&Uuid::nil())
            .await
            .expect("unknown album lookup")
            .is_empty());
        assert!(backend
            .get_artist_tracks(&Uuid::nil())
            .await
            .expect("unknown artist lookup")
            .is_empty());
    }

    #[tokio::test]
    async fn empty_library_has_zero_stable_aggregates() {
        let backend = LocalBackend::new(in_memory_db().await);
        assert!(backend
            .list_albums(SortField::Title, SortOrder::Ascending)
            .await
            .expect("list empty albums")
            .is_empty());
        assert!(backend
            .list_artists()
            .await
            .expect("list empty artists")
            .is_empty());

        let stats = backend.get_stats().await.expect("get empty stats");
        assert_eq!(stats.total_tracks, 0);
        assert_eq!(stats.total_albums, 0);
        assert_eq!(stats.total_artists, 0);
        assert_eq!(stats.total_duration_secs, 0);
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
            .execute(Statement::from_sql_and_values(
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
