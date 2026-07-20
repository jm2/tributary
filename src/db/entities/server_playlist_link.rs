//! SeaORM entity and validated storage boundary for pull-only server playlists.
//!
//! Raw database strings remain private persistence representations. Consumers
//! convert them through [`StoredServerPlaylistLink`] before treating a row as
//! authority-bearing identity or synchronization state.

use std::fmt;

use sea_orm::entity::prelude::*;

use crate::architecture::{NativePlaylistId, SourceId};

/// Frozen persistence token for the only supported synchronization mode.
pub const SERVER_PLAYLIST_LINK_MODE: &str = "pull_read_only_v1";
/// Frozen canonical digest format version.
pub const SERVER_PLAYLIST_DIGEST_VERSION: i32 = 1;
/// Byte length of one SHA-256 membership digest.
pub const SERVER_PLAYLIST_DIGEST_BYTES: usize = 32;
/// Maximum stored byte length of the synchronized local playlist name.
pub const MAX_SERVER_PLAYLIST_LINK_NAME_BYTES: usize = 16 * 1024;
/// Maximum durable local playlist identity length.
pub const MAX_SERVER_PLAYLIST_LOCAL_ID_BYTES: usize = 4 * 1024;
/// Largest UTC Unix-millisecond instant representable through year 9999.
pub const MAX_SERVER_PLAYLIST_SUCCESS_AT_MS: i64 = 253_402_300_799_999;

/// Raw `server_playlist_links` table model.
///
/// A manual [`Debug`] implementation is intentional: the native identity,
/// synchronized name, source identity, and digest can all contain or derive
/// from server-controlled content and must not appear in diagnostics.
#[derive(Clone, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "server_playlist_links")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub playlist_id: String,
    pub source_id: String,
    pub native_playlist_id: String,
    pub mode: String,
    pub last_synced_name: String,
    pub digest_version: i32,
    pub membership_digest: Vec<u8>,
    pub last_success_at_ms: i64,
    pub local_state: String,
    pub remote_state: String,
    pub state_revision: i64,
}

impl fmt::Debug for Model {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistLinkModel")
            .field("playlist_id_byte_len", &self.playlist_id.len())
            .field("source_id_byte_len", &self.source_id.len())
            .field(
                "native_playlist_id_byte_len",
                &self.native_playlist_id.len(),
            )
            .field("mode_byte_len", &self.mode.len())
            .field("last_synced_name_byte_len", &self.last_synced_name.len())
            .field("digest_version", &self.digest_version)
            .field("membership_digest_byte_len", &self.membership_digest.len())
            .field("last_success_at_ms", &self.last_success_at_ms)
            .field("local_state_byte_len", &self.local_state.len())
            .field("remote_state_byte_len", &self.remote_state.len())
            .field("state_revision", &self.state_revision)
            .finish_non_exhaustive()
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::playlist::Entity",
        from = "Column::PlaylistId",
        to = "super::playlist::Column::Id",
        on_delete = "Cascade"
    )]
    Playlist,
}

impl Related<super::playlist::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Playlist.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// Fixed synchronization direction and mutability of a persisted link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistLinkMode {
    PullReadOnly,
}

impl ServerPlaylistLinkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PullReadOnly => SERVER_PLAYLIST_LINK_MODE,
        }
    }
}

impl TryFrom<&str> for ServerPlaylistLinkMode {
    type Error = ServerPlaylistLinkDataError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            SERVER_PLAYLIST_LINK_MODE => Ok(Self::PullReadOnly),
            _ => Err(ServerPlaylistLinkDataError::Mode),
        }
    }
}

/// Durable local-content relationship to the last successful snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistLocalState {
    Clean,
    Conflict,
}

impl ServerPlaylistLocalState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Conflict => "conflict",
        }
    }
}

impl TryFrom<&str> for ServerPlaylistLocalState {
    type Error = ServerPlaylistLinkDataError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "clean" => Ok(Self::Clean),
            "conflict" => Ok(Self::Conflict),
            _ => Err(ServerPlaylistLinkDataError::LocalState),
        }
    }
}

/// Durable server-presence evidence from a complete current listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistRemoteState {
    Present,
    Missing,
}

impl ServerPlaylistRemoteState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
        }
    }
}

impl TryFrom<&str> for ServerPlaylistRemoteState {
    type Error = ServerPlaylistLinkDataError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "present" => Ok(Self::Present),
            "missing" => Ok(Self::Missing),
            _ => Err(ServerPlaylistLinkDataError::RemoteState),
        }
    }
}

/// Canonical, content-redacted representation of one persisted link row.
#[derive(Clone, Eq, PartialEq)]
pub struct StoredServerPlaylistLink {
    pub playlist_id: String,
    pub source_id: SourceId,
    pub native_playlist_id: NativePlaylistId,
    pub mode: ServerPlaylistLinkMode,
    pub last_synced_name: String,
    pub membership_digest: [u8; SERVER_PLAYLIST_DIGEST_BYTES],
    pub last_success_at_ms: i64,
    pub local_state: ServerPlaylistLocalState,
    pub remote_state: ServerPlaylistRemoteState,
    pub state_revision: i64,
}

impl fmt::Debug for StoredServerPlaylistLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredServerPlaylistLink")
            .field("playlist_id_byte_len", &self.playlist_id.len())
            .field("native_playlist_id", &self.native_playlist_id)
            .field("mode", &self.mode)
            .field("last_synced_name_byte_len", &self.last_synced_name.len())
            .field("membership_digest_byte_len", &self.membership_digest.len())
            .field("last_success_at_ms", &self.last_success_at_ms)
            .field("local_state", &self.local_state)
            .field("remote_state", &self.remote_state)
            .field("state_revision", &self.state_revision)
            .finish_non_exhaustive()
    }
}

impl TryFrom<Model> for StoredServerPlaylistLink {
    type Error = ServerPlaylistLinkDataError;

    fn try_from(model: Model) -> Result<Self, Self::Error> {
        if model.playlist_id.is_empty()
            || model.playlist_id.len() > MAX_SERVER_PLAYLIST_LOCAL_ID_BYTES
        {
            return Err(ServerPlaylistLinkDataError::PlaylistIdentity);
        }

        let source_id = model
            .source_id
            .parse::<SourceId>()
            .map_err(|_| ServerPlaylistLinkDataError::SourceIdentity)?;
        if source_id.to_string() != model.source_id || source_id.is_reserved_remote() {
            return Err(ServerPlaylistLinkDataError::SourceIdentity);
        }

        let native_playlist_id = NativePlaylistId::new(model.native_playlist_id)
            .map_err(|_| ServerPlaylistLinkDataError::NativePlaylistIdentity)?;
        let mode = ServerPlaylistLinkMode::try_from(model.mode.as_str())?;
        if model.last_synced_name.len() > MAX_SERVER_PLAYLIST_LINK_NAME_BYTES {
            return Err(ServerPlaylistLinkDataError::LastSyncedName);
        }
        if model.digest_version != SERVER_PLAYLIST_DIGEST_VERSION {
            return Err(ServerPlaylistLinkDataError::DigestVersion);
        }
        let membership_digest = model
            .membership_digest
            .try_into()
            .map_err(|_| ServerPlaylistLinkDataError::MembershipDigest)?;
        if !(0..=MAX_SERVER_PLAYLIST_SUCCESS_AT_MS).contains(&model.last_success_at_ms) {
            return Err(ServerPlaylistLinkDataError::LastSuccessTimestamp);
        }
        let local_state = ServerPlaylistLocalState::try_from(model.local_state.as_str())?;
        let remote_state = ServerPlaylistRemoteState::try_from(model.remote_state.as_str())?;
        if model.state_revision < 0 {
            return Err(ServerPlaylistLinkDataError::StateRevision);
        }

        Ok(Self {
            playlist_id: model.playlist_id,
            source_id,
            native_playlist_id,
            mode,
            last_synced_name: model.last_synced_name,
            membership_digest,
            last_success_at_ms: model.last_success_at_ms,
            local_state,
            remote_state,
            state_revision: model.state_revision,
        })
    }
}

/// Closed, content-free validation failures for persisted native-playlist links.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ServerPlaylistLinkDataError {
    #[error("invalid linked local playlist identity")]
    PlaylistIdentity,
    #[error("invalid linked source identity")]
    SourceIdentity,
    #[error("invalid linked native playlist identity")]
    NativePlaylistIdentity,
    #[error("invalid server-playlist link mode")]
    Mode,
    #[error("invalid synchronized playlist name")]
    LastSyncedName,
    #[error("invalid server-playlist digest version")]
    DigestVersion,
    #[error("invalid server-playlist membership digest")]
    MembershipDigest,
    #[error("invalid server-playlist last-success timestamp")]
    LastSuccessTimestamp,
    #[error("invalid server-playlist local state")]
    LocalState,
    #[error("invalid server-playlist remote state")]
    RemoteState,
    #[error("invalid server-playlist state revision")]
    StateRevision,
}

#[cfg(test)]
mod tests {
    use super::*;

    const REMOTE_SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";

    fn model() -> Model {
        Model {
            playlist_id: "local-playlist".to_string(),
            source_id: REMOTE_SOURCE_ID.to_string(),
            native_playlist_id: " Native/Playlist ☃".to_string(),
            mode: SERVER_PLAYLIST_LINK_MODE.to_string(),
            last_synced_name: " Exact server name ☃".to_string(),
            digest_version: SERVER_PLAYLIST_DIGEST_VERSION,
            membership_digest: vec![0xa5; SERVER_PLAYLIST_DIGEST_BYTES],
            last_success_at_ms: 1_752_937_200_123,
            local_state: "clean".to_string(),
            remote_state: "present".to_string(),
            state_revision: 7,
        }
    }

    #[test]
    fn canonical_row_round_trip_preserves_exact_identity_and_orthogonal_state() {
        let stored = StoredServerPlaylistLink::try_from(model()).expect("canonical link");
        assert_eq!(stored.playlist_id, "local-playlist");
        assert_eq!(stored.source_id.to_string(), REMOTE_SOURCE_ID);
        assert_eq!(stored.native_playlist_id.as_str(), " Native/Playlist ☃");
        assert_eq!(stored.mode, ServerPlaylistLinkMode::PullReadOnly);
        assert_eq!(stored.last_synced_name, " Exact server name ☃");
        assert_eq!(stored.membership_digest, [0xa5; 32]);
        assert_eq!(stored.last_success_at_ms, 1_752_937_200_123);
        assert_eq!(stored.local_state, ServerPlaylistLocalState::Clean);
        assert_eq!(stored.remote_state, ServerPlaylistRemoteState::Present);
        assert_eq!(stored.state_revision, 7);

        let mut independent = model();
        independent.local_state = "conflict".to_string();
        independent.remote_state = "missing".to_string();
        let independent =
            StoredServerPlaylistLink::try_from(independent).expect("orthogonal states");
        assert_eq!(independent.local_state, ServerPlaylistLocalState::Conflict);
        assert_eq!(independent.remote_state, ServerPlaylistRemoteState::Missing);
    }

    #[test]
    fn row_decoder_rejects_every_noncanonical_field() {
        let invalid = [
            (|row: &mut Model| row.playlist_id.clear()) as fn(&mut Model),
            |row: &mut Model| row.playlist_id = "x".repeat(MAX_SERVER_PLAYLIST_LOCAL_ID_BYTES + 1),
            |row: &mut Model| row.source_id = "11111111-1111-4111-8111-11111111111A".to_string(),
            |row: &mut Model| row.source_id = SourceId::local().to_string(),
            |row: &mut Model| row.native_playlist_id.clear(),
            |row: &mut Model| row.mode = "push".to_string(),
            |row: &mut Model| {
                row.last_synced_name = "x".repeat(MAX_SERVER_PLAYLIST_LINK_NAME_BYTES + 1);
            },
            |row: &mut Model| row.digest_version = 2,
            |row: &mut Model| {
                row.membership_digest.pop();
            },
            |row: &mut Model| row.last_success_at_ms = -1,
            |row: &mut Model| row.last_success_at_ms = MAX_SERVER_PLAYLIST_SUCCESS_AT_MS + 1,
            |row: &mut Model| row.local_state = "dirty".to_string(),
            |row: &mut Model| row.remote_state = "offline".to_string(),
            |row: &mut Model| row.state_revision = -1,
        ];

        for mutate in invalid {
            let mut row = model();
            mutate(&mut row);
            assert!(StoredServerPlaylistLink::try_from(row).is_err());
        }
    }

    #[test]
    fn model_domain_and_error_diagnostics_redact_controlled_content() {
        let secret = "SERVER-CONTROLLED-SECRET-93d9";
        let mut raw = model();
        raw.playlist_id = secret.to_string();
        raw.source_id = "33333333-3333-4333-8333-333333333333".to_string();
        raw.native_playlist_id = secret.to_string();
        raw.last_synced_name = secret.to_string();
        raw.membership_digest = secret.as_bytes().to_vec();

        let raw_debug = format!("{raw:?}");
        assert!(!raw_debug.contains(secret));
        assert!(!raw_debug.contains(&raw.source_id));
        assert!(!raw_debug.contains("SERVER-CONTROLLED"));

        let mut valid = model();
        valid.native_playlist_id = secret.to_string();
        valid.last_synced_name = secret.to_string();
        let stored = StoredServerPlaylistLink::try_from(valid).expect("valid secret row");
        let stored_debug = format!("{stored:?}");
        assert!(!stored_debug.contains(secret));
        assert!(!stored_debug.contains(REMOTE_SOURCE_ID));

        let error =
            StoredServerPlaylistLink::try_from(raw).expect_err("wrong digest length must fail");
        let diagnostics = format!("{error:?} {error}");
        assert!(!diagnostics.contains(secret));
        assert!(!diagnostics.contains("SERVER-CONTROLLED"));
    }

    #[test]
    fn exact_field_boundaries_are_accepted() {
        let mut row = model();
        row.playlist_id = "x".repeat(MAX_SERVER_PLAYLIST_LOCAL_ID_BYTES);
        row.native_playlist_id =
            "n".repeat(crate::architecture::identity::MAX_NATIVE_PLAYLIST_ID_BYTES);
        row.last_synced_name = "x".repeat(MAX_SERVER_PLAYLIST_LINK_NAME_BYTES);
        row.last_success_at_ms = MAX_SERVER_PLAYLIST_SUCCESS_AT_MS;
        row.state_revision = i64::MAX;
        StoredServerPlaylistLink::try_from(row).expect("exact maxima");
    }
}
