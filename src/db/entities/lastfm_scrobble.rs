//! SeaORM entity and validated boundary for durable Last.fm scrobbles.
//!
//! Queue rows contain only the bounded metadata Last.fm requires, an opaque
//! playback identity, a one-way account-binding digest, and delivery state.
//! They deliberately contain no media
//! locator, source-native identity, server address, credential, or response
//! body. Manual `Debug` implementations keep listening metadata and timestamps
//! out of diagnostics.

use std::fmt;

use sea_orm::entity::prelude::*;
use uuid::{Uuid, Variant, Version};

/// Fixed byte width of a playback-occurrence identity.
pub const LASTFM_QUEUE_OCCURRENCE_ID_BYTES: usize = 16;
/// Fixed byte width of the one-way account-binding digest.
pub const LASTFM_ACCOUNT_BINDING_BYTES: usize = 32;
/// Maximum UTF-8 byte length of each persisted metadata field.
pub const MAX_LASTFM_METADATA_BYTES: usize = 1024;
/// Last UTC Unix second representable through year 9999.
pub const MAX_LASTFM_STARTED_AT_SECS: i64 = 253_402_300_799;
/// Last UTC Unix millisecond representable through year 9999.
pub const MAX_LASTFM_RETRY_AT_MS: i64 = 253_402_300_799_999;
/// Largest retained retry exponent/counter.
pub const MAX_LASTFM_ATTEMPT_COUNT: i32 = 31;

/// Opaque database representation of a playback-occurrence identity.
///
/// The wrapper preserves the SQLite `BLOB` representation while preventing
/// SeaORM's generated `ActiveModel` `Debug` implementation from printing the
/// bytes.
#[derive(Clone, PartialEq, Eq, DeriveValueType)]
pub struct StoredOccurrenceId(Vec<u8>);

impl StoredOccurrenceId {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for StoredOccurrenceId {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl fmt::Debug for StoredOccurrenceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredOccurrenceId(<redacted>)")
    }
}

/// Opaque database representation of a one-way account-binding digest.
#[derive(Clone, PartialEq, Eq, DeriveValueType)]
pub struct StoredAccountBinding(Vec<u8>);

impl StoredAccountBinding {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for StoredAccountBinding {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl fmt::Debug for StoredAccountBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredAccountBinding(<redacted>)")
    }
}

/// Opaque database representation of persisted listening metadata.
#[derive(Clone, PartialEq, Eq, DeriveValueType)]
pub struct StoredMetadataText(String);

impl StoredMetadataText {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn into_inner(self) -> String {
        self.0
    }
}

impl From<String> for StoredMetadataText {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for StoredMetadataText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredMetadataText(<redacted>)")
    }
}

/// Opaque database representation of an optional private track number.
#[derive(Clone, Copy, PartialEq, Eq, DeriveValueType)]
pub struct StoredTrackNumber(i32);

impl StoredTrackNumber {
    pub(crate) const fn get(self) -> i32 {
        self.0
    }
}

impl From<i32> for StoredTrackNumber {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

impl fmt::Debug for StoredTrackNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredTrackNumber(<redacted>)")
    }
}

/// Opaque database representation of a private exact playback duration.
#[derive(Clone, Copy, PartialEq, Eq, DeriveValueType)]
pub struct StoredDuration(i32);

impl StoredDuration {
    pub(crate) const fn get(self) -> i32 {
        self.0
    }
}

impl From<i32> for StoredDuration {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

impl fmt::Debug for StoredDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredDuration(<redacted>)")
    }
}

/// Opaque database representation of a private playback-start timestamp.
#[derive(Clone, Copy, PartialEq, Eq, DeriveValueType)]
pub struct StoredStartedAt(i64);

impl StoredStartedAt {
    pub(crate) const fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for StoredStartedAt {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl fmt::Debug for StoredStartedAt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredStartedAt(<redacted>)")
    }
}

/// Raw `lastfm_scrobble_queue` row.
#[derive(Clone, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "lastfm_scrobble_queue")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub occurrence_id: StoredOccurrenceId,
    pub account_binding: StoredAccountBinding,
    pub artist: StoredMetadataText,
    pub track_title: StoredMetadataText,
    pub album: Option<StoredMetadataText>,
    pub album_artist: Option<StoredMetadataText>,
    pub track_number: Option<StoredTrackNumber>,
    pub duration_secs: StoredDuration,
    pub started_at_unix_secs: StoredStartedAt,
    pub attempt_count: i32,
    pub next_attempt_at_ms: i64,
}

impl fmt::Debug for Model {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LastFmScrobbleQueueModel")
            .field("id", &self.id)
            .field(
                "occurrence_id_byte_len",
                &self.occurrence_id.as_slice().len(),
            )
            .field(
                "account_binding_byte_len",
                &self.account_binding.as_slice().len(),
            )
            .field("artist_byte_len", &self.artist.as_str().len())
            .field("track_title_byte_len", &self.track_title.as_str().len())
            .field("has_album", &self.album.is_some())
            .field("has_album_artist", &self.album_artist.is_some())
            .field("has_track_number", &self.track_number.is_some())
            .field("attempt_count", &self.attempt_count)
            .finish_non_exhaustive()
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Validated durable row returned to the delivery worker.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredLastFmScrobble {
    pub id: i64,
    pub occurrence_id: Uuid,
    pub account_binding: [u8; LASTFM_ACCOUNT_BINDING_BYTES],
    pub artist: String,
    pub track_title: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<i32>,
    pub duration_secs: i32,
    pub started_at_unix_secs: i64,
    pub attempt_count: i32,
    pub next_attempt_at_ms: i64,
}

impl fmt::Debug for StoredLastFmScrobble {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredLastFmScrobble")
            .field("id", &self.id)
            .field("artist_byte_len", &self.artist.len())
            .field("track_title_byte_len", &self.track_title.len())
            .field("has_album", &self.album.is_some())
            .field("has_album_artist", &self.album_artist.is_some())
            .field("has_track_number", &self.track_number.is_some())
            .field("attempt_count", &self.attempt_count)
            .finish_non_exhaustive()
    }
}

impl TryFrom<Model> for StoredLastFmScrobble {
    type Error = LastFmScrobbleDataError;

    fn try_from(model: Model) -> Result<Self, Self::Error> {
        let Model {
            id,
            occurrence_id,
            account_binding,
            artist,
            track_title,
            album,
            album_artist,
            track_number,
            duration_secs,
            started_at_unix_secs,
            attempt_count,
            next_attempt_at_ms,
        } = model;

        if id <= 0 {
            return Err(LastFmScrobbleDataError::RowIdentity);
        }
        let occurrence_id = canonical_random_uuid(occurrence_id.as_slice())
            .ok_or(LastFmScrobbleDataError::OccurrenceIdentity)?;
        let account_binding = account_binding
            .as_slice()
            .try_into()
            .map_err(|_| LastFmScrobbleDataError::AccountBinding)?;
        validate_required_text(artist.as_str()).map_err(|()| LastFmScrobbleDataError::Artist)?;
        validate_required_text(track_title.as_str())
            .map_err(|()| LastFmScrobbleDataError::TrackTitle)?;
        validate_optional_text(album.as_ref().map(StoredMetadataText::as_str))
            .map_err(|()| LastFmScrobbleDataError::Album)?;
        validate_optional_text(album_artist.as_ref().map(StoredMetadataText::as_str))
            .map_err(|()| LastFmScrobbleDataError::AlbumArtist)?;
        let track_number = track_number.map(StoredTrackNumber::get);
        validate_optional_positive(track_number)
            .map_err(|()| LastFmScrobbleDataError::TrackNumber)?;
        let duration_secs = duration_secs.get();
        if duration_secs <= 30 {
            return Err(LastFmScrobbleDataError::Duration);
        }
        let started_at_unix_secs = started_at_unix_secs.get();
        if !(1..=MAX_LASTFM_STARTED_AT_SECS).contains(&started_at_unix_secs) {
            return Err(LastFmScrobbleDataError::StartedAt);
        }
        if !(0..=MAX_LASTFM_ATTEMPT_COUNT).contains(&attempt_count) {
            return Err(LastFmScrobbleDataError::AttemptCount);
        }
        if !(0..=MAX_LASTFM_RETRY_AT_MS).contains(&next_attempt_at_ms) {
            return Err(LastFmScrobbleDataError::NextAttemptAt);
        }

        Ok(Self {
            id,
            occurrence_id,
            account_binding,
            artist: artist.into_inner(),
            track_title: track_title.into_inner(),
            album: album.map(StoredMetadataText::into_inner),
            album_artist: album_artist.map(StoredMetadataText::into_inner),
            track_number,
            duration_secs,
            started_at_unix_secs,
            attempt_count,
            next_attempt_at_ms,
        })
    }
}

fn canonical_random_uuid(bytes: &[u8]) -> Option<Uuid> {
    if bytes.len() != LASTFM_QUEUE_OCCURRENCE_ID_BYTES {
        return None;
    }
    let uuid = Uuid::from_slice(bytes).ok()?;
    (uuid.get_variant() == Variant::RFC4122 && uuid.get_version() == Some(Version::Random))
        .then_some(uuid)
}

fn validate_required_text(value: &str) -> Result<(), ()> {
    if value.len() > MAX_LASTFM_METADATA_BYTES
        || !value.chars().any(|character| !character.is_whitespace())
        || value.chars().any(char::is_control)
    {
        return Err(());
    }
    Ok(())
}

fn validate_optional_text(value: Option<&str>) -> Result<(), ()> {
    value.map_or(Ok(()), validate_required_text)
}

fn validate_optional_positive(value: Option<i32>) -> Result<(), ()> {
    if value.is_some_and(|value| value <= 0) {
        return Err(());
    }
    Ok(())
}

/// Reason an untrusted queue row was not canonical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastFmScrobbleDataError {
    RowIdentity,
    OccurrenceIdentity,
    AccountBinding,
    Artist,
    TrackTitle,
    Album,
    AlbumArtist,
    TrackNumber,
    Duration,
    StartedAt,
    AttemptCount,
    NextAttemptAt,
}

impl fmt::Display for LastFmScrobbleDataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Last.fm queue row is not canonical")
    }
}

impl std::error::Error for LastFmScrobbleDataError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            id: 1,
            occurrence_id: Uuid::new_v4().as_bytes().to_vec().into(),
            account_binding: vec![0xa5; LASTFM_ACCOUNT_BINDING_BYTES].into(),
            artist: "Artist".to_owned().into(),
            track_title: "Track".to_owned().into(),
            album: Some("Album".to_owned().into()),
            album_artist: None,
            track_number: Some(1.into()),
            duration_secs: 31.into(),
            started_at_unix_secs: 1.into(),
            attempt_count: 0,
            next_attempt_at_ms: 0,
        }
    }

    #[test]
    fn conversion_validates_every_private_storage_dimension() {
        StoredLastFmScrobble::try_from(model()).expect("canonical row");

        let mut cases = Vec::new();
        let mut invalid = model();
        invalid.id = 0;
        cases.push((invalid, LastFmScrobbleDataError::RowIdentity));
        let mut invalid = model();
        invalid.occurrence_id = Uuid::nil().as_bytes().to_vec().into();
        cases.push((invalid, LastFmScrobbleDataError::OccurrenceIdentity));
        let mut invalid = model();
        invalid.account_binding = vec![0xa5; LASTFM_ACCOUNT_BINDING_BYTES - 1].into();
        cases.push((invalid, LastFmScrobbleDataError::AccountBinding));
        let mut invalid = model();
        invalid.artist = " \t".to_owned().into();
        cases.push((invalid, LastFmScrobbleDataError::Artist));
        let mut invalid = model();
        invalid.artist = "Artist\n".to_owned().into();
        cases.push((invalid, LastFmScrobbleDataError::Artist));
        let mut invalid = model();
        invalid.track_title = "\n".to_owned().into();
        cases.push((invalid, LastFmScrobbleDataError::TrackTitle));
        let mut invalid = model();
        invalid.album = Some(String::new().into());
        cases.push((invalid, LastFmScrobbleDataError::Album));
        let mut invalid = model();
        invalid.album_artist = Some("\t".to_owned().into());
        cases.push((invalid, LastFmScrobbleDataError::AlbumArtist));
        let mut invalid = model();
        invalid.track_number = Some(0.into());
        cases.push((invalid, LastFmScrobbleDataError::TrackNumber));
        let mut invalid = model();
        invalid.duration_secs = 30.into();
        cases.push((invalid, LastFmScrobbleDataError::Duration));
        let mut invalid = model();
        invalid.started_at_unix_secs = 0.into();
        cases.push((invalid, LastFmScrobbleDataError::StartedAt));
        let mut invalid = model();
        invalid.attempt_count = MAX_LASTFM_ATTEMPT_COUNT + 1;
        cases.push((invalid, LastFmScrobbleDataError::AttemptCount));
        let mut invalid = model();
        invalid.next_attempt_at_ms = -1;
        cases.push((invalid, LastFmScrobbleDataError::NextAttemptAt));

        for (invalid, expected) in cases {
            assert_eq!(
                StoredLastFmScrobble::try_from(invalid).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn metadata_limits_are_utf8_byte_exact() {
        let mut exact = model();
        exact.artist = "🎵".repeat(MAX_LASTFM_METADATA_BYTES / 4).into();
        StoredLastFmScrobble::try_from(exact.clone()).expect("1,024-byte metadata is accepted");

        exact.artist = format!("{}a", exact.artist.as_str()).into();
        assert_eq!(
            StoredLastFmScrobble::try_from(exact).unwrap_err(),
            LastFmScrobbleDataError::Artist
        );
    }

    #[test]
    fn debug_output_is_content_and_timestamp_free() {
        let mut raw = model();
        raw.occurrence_id = b"private-occurrence-id".to_vec().into();
        raw.account_binding = b"private-account-binding-sentinel!".to_vec().into();
        raw.artist = "PRIVATE_ARTIST_SENTINEL".to_owned().into();
        raw.track_title = "PRIVATE_TRACK_SENTINEL".to_owned().into();
        raw.album = Some("PRIVATE_ALBUM_SENTINEL".to_owned().into());
        raw.album_artist = Some("PRIVATE_ALBUM_ARTIST_SENTINEL".to_owned().into());
        raw.track_number = Some(31_337.into());
        raw.duration_secs = 32_147.into();
        raw.started_at_unix_secs = 1_700_123_456.into();

        // Conversion validation needs a canonical occurrence UUID, so retain a
        // separate valid row for the worker-facing type.
        let mut stored_model = model();
        stored_model.duration_secs = 42_789.into();
        let stored = StoredLastFmScrobble::try_from(stored_model).unwrap();
        let active = ActiveModel::from(raw.clone());
        let private_occurrence = format!("{:?}", raw.occurrence_id.as_slice());
        let private_binding = format!("{:?}", raw.account_binding.as_slice());
        let active_diagnostics = format!("{active:?}");
        for redaction in [
            "StoredOccurrenceId(<redacted>)",
            "StoredAccountBinding(<redacted>)",
            "StoredMetadataText(<redacted>)",
            "StoredTrackNumber(<redacted>)",
            "StoredDuration(<redacted>)",
            "StoredStartedAt(<redacted>)",
        ] {
            assert!(active_diagnostics.contains(redaction));
        }
        let raw_diagnostics = format!("{raw:?}");
        let stored_diagnostics = format!("{stored:?}");
        assert!(!raw_diagnostics.contains("32147"));
        assert!(!active_diagnostics.contains("31337"));
        assert!(!active_diagnostics.contains("32147"));
        assert!(!stored_diagnostics.contains("42789"));
        let diagnostics = [raw_diagnostics, stored_diagnostics, active_diagnostics];

        for diagnostics in diagnostics {
            for sentinel in [
                "PRIVATE_ARTIST_SENTINEL",
                "PRIVATE_TRACK_SENTINEL",
                "PRIVATE_ALBUM_SENTINEL",
                "PRIVATE_ALBUM_ARTIST_SENTINEL",
                "1700123456",
                private_occurrence.as_str(),
                private_binding.as_str(),
            ] {
                assert!(
                    !diagnostics.contains(sentinel),
                    "private value leaked through Debug: {diagnostics}"
                );
            }
        }
    }
}
