//! Atomic persistence for detached and pull-synchronized server playlists.

use std::fmt;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::architecture::server_playlist::MAX_SERVER_PLAYLIST_HINT_BYTES;
use crate::architecture::{NativePlaylistId, ServerPlaylistSnapshot, SourceId, TrackId};
use crate::db::entities::{playlist, playlist_entry, server_playlist_link};
use crate::source_registry::{
    ServerPlaylistAbsenceEvidence, ServerPlaylistCommitAuthority, ServerPlaylistPull,
};

use super::{commit_with_authority, now_rfc3339, require_regular_playlist, PlaylistManager};

pub use crate::db::entities::server_playlist_link::{
    ServerPlaylistLocalState, ServerPlaylistRemoteState,
    StoredServerPlaylistLink as ServerPlaylistLink,
};

use crate::db::entities::server_playlist_link::{
    ServerPlaylistLinkMode, MAX_SERVER_PLAYLIST_SUCCESS_AT_MS, SERVER_PLAYLIST_DIGEST_VERSION,
};

const SERVER_PLAYLIST_ENTRY_INSERT_CHUNK: usize = 64;
const MEMBERSHIP_DIGEST_DOMAIN: &[u8] = b"tributary:server-playlist-membership:v1\0";

/// One ordinary local playlist created from a server-owned snapshot.
#[derive(Clone, Eq, PartialEq)]
pub struct ServerPlaylistLocalCopy {
    playlist_id: String,
    name: String,
    entry_count: usize,
}

impl ServerPlaylistLocalCopy {
    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }
}

impl fmt::Debug for ServerPlaylistLocalCopy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistLocalCopy")
            .field("playlist_id_byte_len", &self.playlist_id.len())
            .field("name_byte_len", &self.name.len())
            .field("entry_count", &self.entry_count)
            .finish_non_exhaustive()
    }
}

/// Exact persisted state captured before a server request begins.
///
/// Applying a pull or absence result consumes a ticket and performs a
/// compare-and-swap against `state_revision`. Cloned tickets may race, but
/// at most one completion for a revision can commit.
#[derive(Clone, Eq, PartialEq)]
pub struct ServerPlaylistSyncTicket {
    playlist_id: String,
    source_id: SourceId,
    native_playlist_id: NativePlaylistId,
    state_revision: i64,
}

impl ServerPlaylistSyncTicket {
    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn native_playlist_id(&self) -> &NativePlaylistId {
        &self.native_playlist_id
    }

    pub const fn state_revision(&self) -> i64 {
        self.state_revision
    }
}

impl fmt::Debug for ServerPlaylistSyncTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistSyncTicket")
            .field("playlist_id_byte_len", &self.playlist_id.len())
            .field("native_playlist_id", &self.native_playlist_id)
            .field("state_revision", &self.state_revision)
            .finish_non_exhaustive()
    }
}

/// Coherent persisted link state and the exact revision ticket to use for one
/// subsequently-started network operation.
#[derive(Clone, Eq, PartialEq)]
pub struct ServerPlaylistSyncPreparation {
    link: ServerPlaylistLink,
    ticket: ServerPlaylistSyncTicket,
}

impl ServerPlaylistSyncPreparation {
    pub const fn link(&self) -> &ServerPlaylistLink {
        &self.link
    }

    pub const fn ticket(&self) -> &ServerPlaylistSyncTicket {
        &self.ticket
    }

    pub fn into_parts(self) -> (ServerPlaylistLink, ServerPlaylistSyncTicket) {
        (self.link, self.ticket)
    }
}

impl fmt::Debug for ServerPlaylistSyncPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistSyncPreparation")
            .field("link", &self.link)
            .field("ticket", &self.ticket)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistPullPolicy {
    RefuseDrift,
    ReplaceLocal,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ServerPlaylistImportOutcome {
    Committed(ServerPlaylistLocalCopy),
    Rejected,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ServerPlaylistCreateOutcome {
    Committed {
        copy: ServerPlaylistLocalCopy,
        link: ServerPlaylistLink,
    },
    AlreadyLinked(ServerPlaylistLink),
    Rejected,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ServerPlaylistPullOutcome {
    Applied {
        copy: ServerPlaylistLocalCopy,
        link: ServerPlaylistLink,
    },
    Conflict(ServerPlaylistLink),
    Superseded,
    Rejected,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ServerPlaylistMissingOutcome {
    Marked(ServerPlaylistLink),
    Superseded,
    Rejected,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ServerPlaylistUnlinkOutcome {
    Unlinked(ServerPlaylistLocalCopy),
    Superseded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistRemoveOutcome {
    Removed,
    Superseded,
}

macro_rules! redacted_outcome_debug {
    ($type:ty, $name:literal, $body:expr) => {
        impl fmt::Debug for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str($body(self, $name))
            }
        }
    };
}

fn import_debug(value: &ServerPlaylistImportOutcome, name: &str) -> &'static str {
    let _ = name;
    match value {
        ServerPlaylistImportOutcome::Committed(_) => "ServerPlaylistImportOutcome::Committed",
        ServerPlaylistImportOutcome::Rejected => "ServerPlaylistImportOutcome::Rejected",
    }
}

fn create_debug(value: &ServerPlaylistCreateOutcome, name: &str) -> &'static str {
    let _ = name;
    match value {
        ServerPlaylistCreateOutcome::Committed { .. } => "ServerPlaylistCreateOutcome::Committed",
        ServerPlaylistCreateOutcome::AlreadyLinked(_) => {
            "ServerPlaylistCreateOutcome::AlreadyLinked"
        }
        ServerPlaylistCreateOutcome::Rejected => "ServerPlaylistCreateOutcome::Rejected",
    }
}

fn pull_debug(value: &ServerPlaylistPullOutcome, name: &str) -> &'static str {
    let _ = name;
    match value {
        ServerPlaylistPullOutcome::Applied { .. } => "ServerPlaylistPullOutcome::Applied",
        ServerPlaylistPullOutcome::Conflict(_) => "ServerPlaylistPullOutcome::Conflict",
        ServerPlaylistPullOutcome::Superseded => "ServerPlaylistPullOutcome::Superseded",
        ServerPlaylistPullOutcome::Rejected => "ServerPlaylistPullOutcome::Rejected",
    }
}

fn missing_debug(value: &ServerPlaylistMissingOutcome, name: &str) -> &'static str {
    let _ = name;
    match value {
        ServerPlaylistMissingOutcome::Marked(_) => "ServerPlaylistMissingOutcome::Marked",
        ServerPlaylistMissingOutcome::Superseded => "ServerPlaylistMissingOutcome::Superseded",
        ServerPlaylistMissingOutcome::Rejected => "ServerPlaylistMissingOutcome::Rejected",
    }
}

fn unlink_debug(value: &ServerPlaylistUnlinkOutcome, name: &str) -> &'static str {
    let _ = name;
    match value {
        ServerPlaylistUnlinkOutcome::Unlinked(_) => "ServerPlaylistUnlinkOutcome::Unlinked",
        ServerPlaylistUnlinkOutcome::Superseded => "ServerPlaylistUnlinkOutcome::Superseded",
    }
}

redacted_outcome_debug!(ServerPlaylistImportOutcome, "import", import_debug);
redacted_outcome_debug!(ServerPlaylistCreateOutcome, "create", create_debug);
redacted_outcome_debug!(ServerPlaylistPullOutcome, "pull", pull_debug);
redacted_outcome_debug!(ServerPlaylistMissingOutcome, "missing", missing_debug);
redacted_outcome_debug!(ServerPlaylistUnlinkOutcome, "unlink", unlink_debug);

impl PlaylistManager {
    /// Load one validated pull-only link without minting live source authority.
    pub async fn get_server_playlist_link(
        &self,
        playlist_id: &str,
    ) -> Result<Option<ServerPlaylistLink>, DbErr> {
        server_playlist_link::Entity::find_by_id(playlist_id.to_string())
            .one(&self.db)
            .await?
            .map(decode_link)
            .transpose()
    }

    /// List every validated link owned by one exact durable source.
    pub async fn list_server_playlist_links(
        &self,
        source_id: SourceId,
    ) -> Result<Vec<ServerPlaylistLink>, DbErr> {
        let rows = server_playlist_link::Entity::find()
            .filter(server_playlist_link::Column::SourceId.eq(source_id.to_string()))
            .order_by_asc(server_playlist_link::Column::PlaylistId)
            .all(&self.db)
            .await?;
        rows.into_iter().map(decode_link).collect()
    }

    /// Capture the exact persisted revision before any server request starts.
    ///
    /// The returned ticket is the only supported input to pull, missing,
    /// unlink, and explicit-removal operations. A completion cannot load a
    /// newer revision after network work and accidentally overwrite it.
    pub async fn prepare_server_playlist_sync(
        &self,
        playlist_id: &str,
    ) -> Result<Option<ServerPlaylistSyncPreparation>, DbErr> {
        let txn = self.db.begin().await?;
        let Some(row) = server_playlist_link::Entity::find_by_id(playlist_id.to_string())
            .one(&txn)
            .await?
        else {
            txn.commit().await?;
            return Ok(None);
        };
        if playlist::Entity::find_by_id(playlist_id.to_string())
            .one(&txn)
            .await?
            .is_none()
        {
            return Err(DbErr::RecordNotFound(
                "Linked playlist not found".to_string(),
            ));
        }
        let link = decode_link(row)?;
        let ticket = ServerPlaylistSyncTicket {
            playlist_id: link.playlist_id.clone(),
            source_id: link.source_id,
            native_playlist_id: link.native_playlist_id.clone(),
            state_revision: link.state_revision,
        };
        txn.commit().await?;
        Ok(Some(ServerPlaylistSyncPreparation { link, ticket }))
    }

    /// Create a detached, immediately editable regular playlist from one
    /// exact-session server snapshot.
    pub async fn import_server_playlist_copy_if_authorized<Authorize>(
        &self,
        pull: &ServerPlaylistPull,
        fallback_name: &str,
        authorize: Authorize,
    ) -> Result<ServerPlaylistImportOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<ServerPlaylistCommitAuthority>,
    {
        self.import_server_playlist_copy_from_snapshot_if_authorized(
            pull.source_id(),
            pull.snapshot(),
            fallback_name,
            || {
                let authority = authorize()?;
                pull.accepts_commit_authority(&authority)
                    .then_some(authority)
            },
        )
        .await
    }

    async fn import_server_playlist_copy_from_snapshot_if_authorized<Authority, Authorize>(
        &self,
        source_id: SourceId,
        snapshot: &ServerPlaylistSnapshot,
        fallback_name: &str,
        authorize: Authorize,
    ) -> Result<ServerPlaylistImportOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<Authority>,
        Authority: Send + 'static,
    {
        validate_snapshot_source(source_id, snapshot)?;
        let name = initial_snapshot_name(snapshot, fallback_name)?;
        let txn = self.db.begin().await?;
        let playlist = insert_regular_playlist(&txn, &name).await?;
        insert_server_snapshot_entries(&txn, &playlist.id, source_id, snapshot.track_ids()).await?;

        let Some(authority) = authorize() else {
            txn.rollback().await?;
            return Ok(ServerPlaylistImportOutcome::Rejected);
        };
        commit_with_authority(txn, authority).await?;
        Ok(ServerPlaylistImportOutcome::Committed(
            ServerPlaylistLocalCopy {
                playlist_id: playlist.id,
                name,
                entry_count: snapshot.track_ids().len(),
            },
        ))
    }

    /// Create one unique pull-only mirror from a current server snapshot.
    ///
    /// The unique `(source_id, native_playlist_id)` schema key remains the
    /// final race arbiter. A sequential duplicate returns the existing link;
    /// a concurrent losing insertion rolls its newly-created playlist and
    /// every staged entry back with the surrounding transaction.
    pub async fn create_server_playlist_mirror_if_authorized<Authorize>(
        &self,
        pull: &ServerPlaylistPull,
        fallback_name: &str,
        authorize: Authorize,
    ) -> Result<ServerPlaylistCreateOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<ServerPlaylistCommitAuthority>,
    {
        self.create_server_playlist_mirror_from_snapshot_if_authorized(
            pull.source_id(),
            pull.snapshot(),
            fallback_name,
            || {
                let authority = authorize()?;
                pull.accepts_commit_authority(&authority)
                    .then_some(authority)
            },
        )
        .await
    }

    async fn create_server_playlist_mirror_from_snapshot_if_authorized<Authority, Authorize>(
        &self,
        source_id: SourceId,
        snapshot: &ServerPlaylistSnapshot,
        fallback_name: &str,
        authorize: Authorize,
    ) -> Result<ServerPlaylistCreateOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<Authority>,
        Authority: Send + 'static,
    {
        validate_snapshot_source(source_id, snapshot)?;
        let name = initial_snapshot_name(snapshot, fallback_name)?;
        let txn = self.db.begin().await?;
        if let Some(existing) = find_link_by_native(&txn, source_id, snapshot.native_id()).await? {
            txn.rollback().await?;
            return Ok(ServerPlaylistCreateOutcome::AlreadyLinked(existing));
        }

        let playlist = insert_regular_playlist(&txn, &name).await?;
        insert_server_snapshot_entries(&txn, &playlist.id, source_id, snapshot.track_ids()).await?;
        let digest = digest_snapshot(source_id, snapshot.track_ids());
        let last_success_at_ms = current_success_timestamp()?;
        let row = server_playlist_link::ActiveModel {
            playlist_id: Set(playlist.id.clone()),
            source_id: Set(source_id.to_string()),
            native_playlist_id: Set(snapshot.native_id().as_str().to_string()),
            mode: Set(ServerPlaylistLinkMode::PullReadOnly.as_str().to_string()),
            last_synced_name: Set(name.clone()),
            digest_version: Set(SERVER_PLAYLIST_DIGEST_VERSION),
            membership_digest: Set(digest.to_vec()),
            last_success_at_ms: Set(last_success_at_ms),
            local_state: Set(ServerPlaylistLocalState::Clean.as_str().to_string()),
            remote_state: Set(ServerPlaylistRemoteState::Present.as_str().to_string()),
            state_revision: Set(0),
        }
        .insert(&txn)
        .await?;
        let link = decode_link(row)?;

        let Some(authority) = authorize() else {
            txn.rollback().await?;
            return Ok(ServerPlaylistCreateOutcome::Rejected);
        };
        commit_with_authority(txn, authority).await?;
        Ok(ServerPlaylistCreateOutcome::Committed {
            copy: ServerPlaylistLocalCopy {
                playlist_id: playlist.id,
                name,
                entry_count: snapshot.track_ids().len(),
            },
            link,
        })
    }

    /// Apply a fresh detail snapshot only to the exact prepared link revision.
    pub async fn apply_server_playlist_pull_if_authorized<Authorize>(
        &self,
        ticket: ServerPlaylistSyncTicket,
        pull: &ServerPlaylistPull,
        policy: ServerPlaylistPullPolicy,
        authorize: Authorize,
    ) -> Result<ServerPlaylistPullOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<ServerPlaylistCommitAuthority>,
    {
        self.apply_server_playlist_snapshot_if_authorized(
            ticket,
            pull.source_id(),
            pull.snapshot(),
            policy,
            || {
                let authority = authorize()?;
                pull.accepts_commit_authority(&authority)
                    .then_some(authority)
            },
        )
        .await
    }

    /// Explicit conflict resolution which discards local divergence in favor
    /// of one newly-fetched current server snapshot.
    pub async fn replace_local_with_server_if_authorized<Authorize>(
        &self,
        ticket: ServerPlaylistSyncTicket,
        pull: &ServerPlaylistPull,
        authorize: Authorize,
    ) -> Result<ServerPlaylistPullOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<ServerPlaylistCommitAuthority>,
    {
        self.apply_server_playlist_pull_if_authorized(
            ticket,
            pull,
            ServerPlaylistPullPolicy::ReplaceLocal,
            authorize,
        )
        .await
    }

    async fn apply_server_playlist_snapshot_if_authorized<Authority, Authorize>(
        &self,
        ticket: ServerPlaylistSyncTicket,
        source_id: SourceId,
        snapshot: &ServerPlaylistSnapshot,
        policy: ServerPlaylistPullPolicy,
        authorize: Authorize,
    ) -> Result<ServerPlaylistPullOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<Authority>,
        Authority: Send + 'static,
    {
        validate_snapshot_source(source_id, snapshot)?;
        validate_ticket_identity(&ticket, source_id, snapshot.native_id())?;
        let txn = self.db.begin().await?;
        let Some(link) = load_ticket_link(&txn, &ticket).await? else {
            txn.rollback().await?;
            return Ok(ServerPlaylistPullOutcome::Superseded);
        };
        require_regular_playlist(&txn, &ticket.playlist_id).await?;
        let playlist = playlist::Entity::find_by_id(ticket.playlist_id.clone())
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("Linked playlist not found".to_string()))?;
        let current_entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(ticket.playlist_id.as_str()))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&txn)
            .await?;
        let current_digest = digest_rows(&current_entries);
        let local_drift =
            playlist.name != link.last_synced_name || current_digest != link.membership_digest;
        let next_revision = next_revision(link.state_revision)?;

        if local_drift && policy == ServerPlaylistPullPolicy::RefuseDrift {
            let update = server_playlist_link::ActiveModel {
                local_state: Set(ServerPlaylistLocalState::Conflict.as_str().to_string()),
                remote_state: Set(ServerPlaylistRemoteState::Present.as_str().to_string()),
                state_revision: Set(next_revision),
                ..Default::default()
            };
            let Some(conflict) = cas_update_link(&txn, &ticket, update).await? else {
                txn.rollback().await?;
                return Ok(ServerPlaylistPullOutcome::Superseded);
            };
            let Some(authority) = authorize() else {
                txn.rollback().await?;
                return Ok(ServerPlaylistPullOutcome::Rejected);
            };
            commit_with_authority(txn, authority).await?;
            return Ok(ServerPlaylistPullOutcome::Conflict(conflict));
        }

        let desired_name = snapshot
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| link.last_synced_name.clone());
        validate_synced_name(&desired_name)?;
        let desired_digest = digest_snapshot(source_id, snapshot.track_ids());
        if desired_digest != current_digest {
            playlist_entry::Entity::delete_many()
                .filter(playlist_entry::Column::PlaylistId.eq(ticket.playlist_id.as_str()))
                .exec(&txn)
                .await?;
            insert_server_snapshot_entries(
                &txn,
                &ticket.playlist_id,
                source_id,
                snapshot.track_ids(),
            )
            .await?;
        }
        if desired_name != playlist.name {
            let mut active: playlist::ActiveModel = playlist.into();
            active.name = Set(desired_name.clone());
            active.updated_at = Set(now_rfc3339());
            active.update(&txn).await?;
        }

        let update = server_playlist_link::ActiveModel {
            last_synced_name: Set(desired_name.clone()),
            digest_version: Set(SERVER_PLAYLIST_DIGEST_VERSION),
            membership_digest: Set(desired_digest.to_vec()),
            last_success_at_ms: Set(current_success_timestamp()?),
            local_state: Set(ServerPlaylistLocalState::Clean.as_str().to_string()),
            remote_state: Set(ServerPlaylistRemoteState::Present.as_str().to_string()),
            state_revision: Set(next_revision),
            ..Default::default()
        };
        let Some(updated) = cas_update_link(&txn, &ticket, update).await? else {
            txn.rollback().await?;
            return Ok(ServerPlaylistPullOutcome::Superseded);
        };
        let Some(authority) = authorize() else {
            txn.rollback().await?;
            return Ok(ServerPlaylistPullOutcome::Rejected);
        };
        commit_with_authority(txn, authority).await?;
        Ok(ServerPlaylistPullOutcome::Applied {
            copy: ServerPlaylistLocalCopy {
                playlist_id: ticket.playlist_id,
                name: desired_name,
                entry_count: snapshot.track_ids().len(),
            },
            link: updated,
        })
    }

    /// Persist exact server absence only when it was proven by a successful
    /// complete listing and the pre-request revision remains current.
    pub async fn mark_server_playlist_missing_if_authorized<Authorize>(
        &self,
        ticket: ServerPlaylistSyncTicket,
        evidence: &ServerPlaylistAbsenceEvidence,
        authorize: Authorize,
    ) -> Result<ServerPlaylistMissingOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<ServerPlaylistCommitAuthority>,
    {
        self.mark_server_playlist_missing_identity_if_authorized(
            ticket,
            evidence.source_id(),
            evidence.native_id(),
            || {
                let authority = authorize()?;
                evidence
                    .accepts_commit_authority(&authority)
                    .then_some(authority)
            },
        )
        .await
    }

    async fn mark_server_playlist_missing_identity_if_authorized<Authority, Authorize>(
        &self,
        ticket: ServerPlaylistSyncTicket,
        source_id: SourceId,
        native_id: &NativePlaylistId,
        authorize: Authorize,
    ) -> Result<ServerPlaylistMissingOutcome, DbErr>
    where
        Authorize: FnOnce() -> Option<Authority>,
        Authority: Send + 'static,
    {
        validate_ticket_identity(&ticket, source_id, native_id)?;
        let txn = self.db.begin().await?;
        let Some(link) = load_ticket_link(&txn, &ticket).await? else {
            txn.rollback().await?;
            return Ok(ServerPlaylistMissingOutcome::Superseded);
        };
        require_regular_playlist(&txn, &ticket.playlist_id).await?;
        let playlist = playlist::Entity::find_by_id(ticket.playlist_id.clone())
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("Linked playlist not found".to_string()))?;
        let current_entries = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(ticket.playlist_id.as_str()))
            .order_by_asc(playlist_entry::Column::Position)
            .all(&txn)
            .await?;
        let local_state = if playlist.name == link.last_synced_name
            && digest_rows(&current_entries) == link.membership_digest
        {
            ServerPlaylistLocalState::Clean
        } else {
            ServerPlaylistLocalState::Conflict
        };
        let update = server_playlist_link::ActiveModel {
            local_state: Set(local_state.as_str().to_string()),
            remote_state: Set(ServerPlaylistRemoteState::Missing.as_str().to_string()),
            state_revision: Set(next_revision(link.state_revision)?),
            ..Default::default()
        };
        let Some(updated) = cas_update_link(&txn, &ticket, update).await? else {
            txn.rollback().await?;
            return Ok(ServerPlaylistMissingOutcome::Superseded);
        };
        let Some(authority) = authorize() else {
            txn.rollback().await?;
            return Ok(ServerPlaylistMissingOutcome::Rejected);
        };
        commit_with_authority(txn, authority).await?;
        Ok(ServerPlaylistMissingOutcome::Marked(updated))
    }

    /// Remove only the pull link at an exact observed revision, retaining the
    /// current local playlist and every occurrence as an editable copy.
    pub async fn unlink_server_playlist(
        &self,
        ticket: ServerPlaylistSyncTicket,
    ) -> Result<ServerPlaylistUnlinkOutcome, DbErr> {
        let txn = self.db.begin().await?;
        if load_ticket_link(&txn, &ticket).await?.is_none() {
            txn.rollback().await?;
            return Ok(ServerPlaylistUnlinkOutcome::Superseded);
        }
        let playlist = playlist::Entity::find_by_id(ticket.playlist_id.clone())
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("Linked playlist not found".to_string()))?;
        let entry_count = playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(ticket.playlist_id.as_str()))
            .count(&txn)
            .await?;
        let entry_count = usize::try_from(entry_count)
            .map_err(|_| DbErr::Custom("Linked playlist is too large".to_string()))?;
        if delete_ticket_link(&txn, &ticket).await? != 1 {
            txn.rollback().await?;
            return Ok(ServerPlaylistUnlinkOutcome::Superseded);
        }
        txn.commit().await?;
        Ok(ServerPlaylistUnlinkOutcome::Unlinked(
            ServerPlaylistLocalCopy {
                playlist_id: playlist.id,
                name: playlist.name,
                entry_count,
            },
        ))
    }

    /// Explicitly delete one linked local copy and its entries at the exact
    /// revision shown to the caller.
    pub async fn remove_local_server_playlist(
        &self,
        ticket: ServerPlaylistSyncTicket,
    ) -> Result<ServerPlaylistRemoveOutcome, DbErr> {
        let txn = self.db.begin().await?;
        if load_ticket_link(&txn, &ticket).await?.is_none() {
            txn.rollback().await?;
            return Ok(ServerPlaylistRemoveOutcome::Superseded);
        }
        if delete_ticket_link(&txn, &ticket).await? != 1 {
            txn.rollback().await?;
            return Ok(ServerPlaylistRemoveOutcome::Superseded);
        }
        let deleted = playlist::Entity::delete_by_id(ticket.playlist_id)
            .exec(&txn)
            .await?;
        if deleted.rows_affected != 1 {
            return Err(DbErr::RecordNotFound(
                "Linked playlist not found".to_string(),
            ));
        }
        txn.commit().await?;
        Ok(ServerPlaylistRemoveOutcome::Removed)
    }
}

fn decode_link(row: server_playlist_link::Model) -> Result<ServerPlaylistLink, DbErr> {
    ServerPlaylistLink::try_from(row)
        .map_err(|error| DbErr::Custom(format!("Invalid server-playlist link: {error}")))
}

async fn find_link_by_native<C>(
    db: &C,
    source_id: SourceId,
    native_id: &NativePlaylistId,
) -> Result<Option<ServerPlaylistLink>, DbErr>
where
    C: ConnectionTrait,
{
    server_playlist_link::Entity::find()
        .filter(server_playlist_link::Column::SourceId.eq(source_id.to_string()))
        .filter(server_playlist_link::Column::NativePlaylistId.eq(native_id.as_str()))
        .one(db)
        .await?
        .map(decode_link)
        .transpose()
}

async fn load_ticket_link(
    txn: &DatabaseTransaction,
    ticket: &ServerPlaylistSyncTicket,
) -> Result<Option<ServerPlaylistLink>, DbErr> {
    let Some(row) = server_playlist_link::Entity::find_by_id(ticket.playlist_id.clone())
        .one(txn)
        .await?
    else {
        return Ok(None);
    };
    let link = decode_link(row)?;
    if link.source_id != ticket.source_id || link.native_playlist_id != ticket.native_playlist_id {
        return Err(DbErr::Custom(
            "Server-playlist link identity changed".to_string(),
        ));
    }
    Ok((link.state_revision == ticket.state_revision).then_some(link))
}

async fn cas_update_link(
    txn: &DatabaseTransaction,
    ticket: &ServerPlaylistSyncTicket,
    update: server_playlist_link::ActiveModel,
) -> Result<Option<ServerPlaylistLink>, DbErr> {
    let changed = server_playlist_link::Entity::update_many()
        .set(update)
        .filter(server_playlist_link::Column::PlaylistId.eq(ticket.playlist_id.as_str()))
        .filter(server_playlist_link::Column::SourceId.eq(ticket.source_id.to_string()))
        .filter(
            server_playlist_link::Column::NativePlaylistId.eq(ticket.native_playlist_id.as_str()),
        )
        .filter(server_playlist_link::Column::StateRevision.eq(ticket.state_revision))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Ok(None);
    }
    server_playlist_link::Entity::find_by_id(ticket.playlist_id.clone())
        .one(txn)
        .await?
        .map(decode_link)
        .transpose()
}

async fn delete_ticket_link(
    txn: &DatabaseTransaction,
    ticket: &ServerPlaylistSyncTicket,
) -> Result<u64, DbErr> {
    Ok(server_playlist_link::Entity::delete_many()
        .filter(server_playlist_link::Column::PlaylistId.eq(ticket.playlist_id.as_str()))
        .filter(server_playlist_link::Column::SourceId.eq(ticket.source_id.to_string()))
        .filter(
            server_playlist_link::Column::NativePlaylistId.eq(ticket.native_playlist_id.as_str()),
        )
        .filter(server_playlist_link::Column::StateRevision.eq(ticket.state_revision))
        .exec(txn)
        .await?
        .rows_affected)
}

async fn insert_regular_playlist(
    txn: &DatabaseTransaction,
    name: &str,
) -> Result<playlist::Model, DbErr> {
    let now = now_rfc3339();
    playlist::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        name: Set(name.to_string()),
        is_smart: Set(false),
        smart_rules_json: Set(None),
        limit_enabled: Set(false),
        limit_value: Set(None),
        limit_unit: Set(None),
        limit_sort: Set(None),
        match_mode: Set("all".to_string()),
        live_updating: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    }
    .insert(txn)
    .await
}

async fn insert_server_snapshot_entries(
    txn: &DatabaseTransaction,
    playlist_id: &str,
    source_id: SourceId,
    track_ids: &[TrackId],
) -> Result<(), DbErr> {
    for (chunk_index, chunk) in track_ids
        .chunks(SERVER_PLAYLIST_ENTRY_INSERT_CHUNK)
        .enumerate()
    {
        let first = chunk_index
            .checked_mul(SERVER_PLAYLIST_ENTRY_INSERT_CHUNK)
            .ok_or_else(|| DbErr::Custom("Server playlist is too large".to_string()))?;
        let mut rows = Vec::with_capacity(chunk.len());
        for (offset, track_id) in chunk.iter().enumerate() {
            TrackId::remote(track_id.as_str()).map_err(|_| {
                DbErr::Custom("Server playlist contains an invalid track identity".to_string())
            })?;
            let position = first
                .checked_add(offset)
                .and_then(|position| i32::try_from(position).ok())
                .ok_or_else(|| DbErr::Custom("Server playlist is too large".to_string()))?;
            rows.push(playlist_entry::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                playlist_id: Set(playlist_id.to_string()),
                position: Set(position),
                source_id: Set(source_id.to_string()),
                track_id: Set(Some(track_id.as_str().to_string())),
                local_track_id: Set(None),
                match_title: Set(String::new()),
                match_artist: Set(String::new()),
                match_album: Set(String::new()),
                match_duration_secs: Set(None),
                match_file_path: Set(None),
            });
        }
        if !rows.is_empty() {
            let inserted = playlist_entry::Entity::insert_many(rows)
                .exec_without_returning(txn)
                .await?;
            let expected = u64::try_from(chunk.len())
                .map_err(|_| DbErr::Custom("Server playlist is too large".to_string()))?;
            if inserted != expected {
                return Err(DbErr::Custom(
                    "Server playlist entry batch was not inserted completely".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn initial_snapshot_name(
    snapshot: &ServerPlaylistSnapshot,
    fallback_name: &str,
) -> Result<String, DbErr> {
    let name = snapshot.name().unwrap_or(fallback_name);
    validate_synced_name(name)?;
    Ok(name.to_string())
}

fn validate_synced_name(name: &str) -> Result<(), DbErr> {
    if name.len() > MAX_SERVER_PLAYLIST_HINT_BYTES {
        return Err(DbErr::Custom(
            "Server playlist name exceeds the storage limit".to_string(),
        ));
    }
    Ok(())
}

fn validate_snapshot_source(
    source_id: SourceId,
    snapshot: &ServerPlaylistSnapshot,
) -> Result<(), DbErr> {
    if source_id.is_reserved_remote() {
        return Err(DbErr::Custom(
            "Server playlist source identity is invalid".to_string(),
        ));
    }
    NativePlaylistId::new(snapshot.native_id().as_str())
        .map_err(|_| DbErr::Custom("Server playlist identity is invalid".to_string()))?;
    for track_id in snapshot.track_ids() {
        TrackId::remote(track_id.as_str()).map_err(|_| {
            DbErr::Custom("Server playlist contains an invalid track identity".to_string())
        })?;
    }
    Ok(())
}

fn validate_ticket_identity(
    ticket: &ServerPlaylistSyncTicket,
    source_id: SourceId,
    native_id: &NativePlaylistId,
) -> Result<(), DbErr> {
    if ticket.source_id != source_id || &ticket.native_playlist_id != native_id {
        return Err(DbErr::Custom(
            "Server playlist result does not match the prepared link".to_string(),
        ));
    }
    Ok(())
}

fn next_revision(current: i64) -> Result<i64, DbErr> {
    current
        .checked_add(1)
        .ok_or_else(|| DbErr::Custom("Server playlist revision is exhausted".to_string()))
}

fn current_success_timestamp() -> Result<i64, DbErr> {
    let timestamp = Utc::now().timestamp_millis();
    if !(0..=MAX_SERVER_PLAYLIST_SUCCESS_AT_MS).contains(&timestamp) {
        return Err(DbErr::Custom(
            "Current time cannot be stored for server playlist sync".to_string(),
        ));
    }
    Ok(timestamp)
}

fn digest_snapshot(source_id: SourceId, track_ids: &[TrackId]) -> [u8; 32] {
    let source_id = source_id.to_string();
    digest_membership(
        track_ids.len(),
        track_ids
            .iter()
            .map(|track_id| (source_id.as_str(), Some(track_id.as_str()))),
    )
}

fn digest_rows(rows: &[playlist_entry::Model]) -> [u8; 32] {
    digest_membership(
        rows.len(),
        rows.iter()
            .map(|row| (row.source_id.as_str(), row.track_id.as_deref())),
    )
}

fn digest_membership<'a>(
    count: usize,
    occurrences: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(MEMBERSHIP_DIGEST_DOMAIN);
    digest.update(u64::try_from(count).unwrap_or(u64::MAX).to_be_bytes());
    for (source_id, track_id) in occurrences {
        digest_component(&mut digest, source_id.as_bytes());
        match track_id {
            Some(track_id) => {
                digest.update([1]);
                digest_component(&mut digest, track_id.as_bytes());
            }
            None => digest.update([0]),
        }
    }
    digest.finalize().into()
}

fn digest_component(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ConnectionTrait, Database, DatabaseConnection,
        EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    };
    use sea_orm_migration::MigratorTrait;

    use super::*;
    use crate::architecture::MediaKey;
    use crate::db::entities::track;
    use crate::db::migration::Migrator;
    use crate::local::playlist_manager::PlaylistEntryInput;
    use crate::local::smart_rules::{MatchMode, SmartRules};

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open test database");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    fn source_id() -> SourceId {
        "11111111-1111-4111-8111-111111111111"
            .parse()
            .expect("remote source ID")
    }

    fn snapshot(native_id: &str, name: Option<&str>, track_ids: &[&str]) -> ServerPlaylistSnapshot {
        ServerPlaylistSnapshot::new(
            NativePlaylistId::new(native_id).expect("native playlist ID"),
            name.map(str::to_string),
            None,
            None,
            track_ids
                .iter()
                .map(|track_id| TrackId::remote(*track_id).expect("remote track ID"))
                .collect(),
        )
        .expect("server playlist snapshot")
    }

    async fn create_mirror(
        manager: &PlaylistManager,
        snapshot: &ServerPlaylistSnapshot,
    ) -> (ServerPlaylistLocalCopy, ServerPlaylistLink) {
        match manager
            .create_server_playlist_mirror_from_snapshot_if_authorized(
                source_id(),
                snapshot,
                "Fallback",
                || Some(()),
            )
            .await
            .expect("create mirror")
        {
            ServerPlaylistCreateOutcome::Committed { copy, link } => (copy, link),
            other => panic!("unexpected mirror result: {other:?}"),
        }
    }

    async fn entries(db: &DatabaseConnection, playlist_id: &str) -> Vec<playlist_entry::Model> {
        playlist_entry::Entity::find()
            .filter(playlist_entry::Column::PlaylistId.eq(playlist_id))
            .order_by_asc(playlist_entry::Column::Position)
            .all(db)
            .await
            .expect("load playlist entries")
    }

    async fn preparation(
        manager: &PlaylistManager,
        playlist_id: &str,
    ) -> ServerPlaylistSyncPreparation {
        manager
            .prepare_server_playlist_sync(playlist_id)
            .await
            .expect("prepare sync")
            .expect("linked playlist")
    }

    async fn direct_rename(db: &DatabaseConnection, playlist_id: &str, name: &str) {
        let model = playlist::Entity::find_by_id(playlist_id)
            .one(db)
            .await
            .expect("load playlist")
            .expect("playlist exists");
        let mut active: playlist::ActiveModel = model.into();
        active.name = Set(name.to_string());
        active.update(db).await.expect("bypass manager rename");
    }

    #[test]
    fn membership_digest_is_frozen_ordered_and_duplicate_preserving() {
        let source = source_id();
        let ordered = [
            TrackId::remote("first").unwrap(),
            TrackId::remote("second").unwrap(),
            TrackId::remote("first").unwrap(),
        ];
        let digest = digest_snapshot(source, &ordered);
        assert_eq!(
            digest,
            [
                125, 113, 90, 50, 148, 213, 63, 245, 214, 28, 128, 70, 22, 179, 114, 88, 2, 247,
                21, 44, 32, 197, 156, 209, 66, 118, 170, 163, 214, 104, 70, 109,
            ],
            "update this golden only with an explicit digest-format migration"
        );
        assert_ne!(
            digest,
            digest_snapshot(
                source,
                &[ordered[1].clone(), ordered[0].clone(), ordered[2].clone()]
            )
        );
        assert_ne!(digest, digest_snapshot(source, &ordered[..2]));
    }

    #[tokio::test]
    async fn detached_import_is_editable_exact_chunked_and_authority_rejection_rolls_back() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let ids: Vec<String> = (0..130)
            .map(|index| format!("track-{}", index % 67))
            .collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let snapshot = snapshot("native-import", Some("Secret imported name"), &id_refs);
        let outcome = manager
            .import_server_playlist_copy_from_snapshot_if_authorized(
                source_id(),
                &snapshot,
                "unused",
                || Some(()),
            )
            .await
            .expect("import detached copy");
        let ServerPlaylistImportOutcome::Committed(copy) = outcome else {
            panic!("import must commit")
        };
        assert_eq!(copy.entry_count(), 130);
        assert!(manager
            .get_server_playlist_link(copy.playlist_id())
            .await
            .unwrap()
            .is_none());
        let stored = entries(&db, copy.playlist_id()).await;
        assert_eq!(
            stored
                .iter()
                .map(|row| row.track_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            id_refs
        );
        manager
            .rename_playlist(copy.playlist_id(), "Editable detached copy")
            .await
            .expect("detached import is editable");

        let playlist_count = playlist::Entity::find().count(&db).await.unwrap();
        let rejected = manager
            .import_server_playlist_copy_from_snapshot_if_authorized(
                source_id(),
                &snapshot,
                "unused",
                Option::<()>::default,
            )
            .await
            .expect("authority denial is typed");
        assert_eq!(rejected, ServerPlaylistImportOutcome::Rejected);
        assert_eq!(
            playlist::Entity::find().count(&db).await.unwrap(),
            playlist_count
        );
    }

    #[tokio::test]
    async fn chunk_failure_rolls_back_the_playlist_and_every_prior_batch_before_authority() {
        let db = database().await;
        db.execute_unprepared(
            "CREATE TRIGGER fail_server_playlist_chunk
             BEFORE INSERT ON playlist_entries WHEN NEW.position = 64
             BEGIN SELECT RAISE(ABORT, 'injected chunk failure'); END",
        )
        .await
        .expect("install failure trigger");
        let manager = PlaylistManager::new(db.clone());
        let ids: Vec<String> = (0..65).map(|index| format!("track-{index}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let snapshot = snapshot("chunk-failure", Some("Chunk failure"), &refs);
        let authorize_called = AtomicBool::new(false);
        let result = manager
            .import_server_playlist_copy_from_snapshot_if_authorized(
                source_id(),
                &snapshot,
                "unused",
                || {
                    authorize_called.store(true, Ordering::Release);
                    Some(())
                },
            )
            .await;
        assert!(result.is_err());
        assert!(!authorize_called.load(Ordering::Acquire));
        assert_eq!(playlist::Entity::find().count(&db).await.unwrap(), 0);
        assert_eq!(playlist_entry::Entity::find().count(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rejected_mirror_rolls_back_every_row_and_nameless_fallback_is_validated() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let nameless = snapshot("nameless-mirror", None, &["a", "b"]);

        assert_eq!(
            manager
                .create_server_playlist_mirror_from_snapshot_if_authorized(
                    source_id(),
                    &nameless,
                    "Fallback",
                    Option::<()>::default,
                )
                .await
                .unwrap(),
            ServerPlaylistCreateOutcome::Rejected
        );
        assert_eq!(playlist::Entity::find().count(&db).await.unwrap(), 0);
        assert_eq!(playlist_entry::Entity::find().count(&db).await.unwrap(), 0);
        assert_eq!(
            server_playlist_link::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );

        let committed = manager
            .create_server_playlist_mirror_from_snapshot_if_authorized(
                source_id(),
                &nameless,
                "Fallback",
                || Some(()),
            )
            .await
            .unwrap();
        let ServerPlaylistCreateOutcome::Committed { copy, link } = committed else {
            panic!("authorized mirror must commit")
        };
        assert_eq!(copy.name(), "Fallback");
        assert_eq!(link.last_synced_name, "Fallback");

        let oversized_fallback = "x".repeat(MAX_SERVER_PLAYLIST_HINT_BYTES + 1);
        let another_nameless = snapshot("oversized-fallback", None, &["c"]);
        assert!(manager
            .create_server_playlist_mirror_from_snapshot_if_authorized(
                source_id(),
                &another_nameless,
                &oversized_fallback,
                || Some(()),
            )
            .await
            .is_err());
        assert_eq!(playlist::Entity::find().count(&db).await.unwrap(), 1);
        assert_eq!(playlist_entry::Entity::find().count(&db).await.unwrap(), 2);
        assert_eq!(
            server_playlist_link::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn pull_chunk_failure_retains_the_complete_previous_snapshot_and_link() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let initial = snapshot("pull-chunk-failure", Some("Baseline"), &["old-a", "old-b"]);
        let (copy, baseline_link) = create_mirror(&manager, &initial).await;
        let baseline_entries = entries(&db, copy.playlist_id()).await;
        let ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();

        db.execute_unprepared(
            "CREATE TRIGGER fail_server_playlist_pull_chunk
             BEFORE INSERT ON playlist_entries WHEN NEW.position = 64
             BEGIN SELECT RAISE(ABORT, 'injected pull chunk failure'); END",
        )
        .await
        .expect("install failure trigger");
        let ids: Vec<String> = (0..65).map(|index| format!("new-track-{index}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let incoming = snapshot("pull-chunk-failure", Some("Incoming"), &refs);
        let authorize_called = AtomicBool::new(false);
        let result = manager
            .apply_server_playlist_snapshot_if_authorized(
                ticket,
                source_id(),
                &incoming,
                ServerPlaylistPullPolicy::ReplaceLocal,
                || {
                    authorize_called.store(true, Ordering::Release);
                    Some(())
                },
            )
            .await;

        assert!(result.is_err());
        assert!(!authorize_called.load(Ordering::Acquire));
        assert_eq!(entries(&db, copy.playlist_id()).await, baseline_entries);
        assert_eq!(
            manager
                .get_server_playlist_link(copy.playlist_id())
                .await
                .unwrap(),
            Some(baseline_link)
        );
        assert_eq!(
            playlist::Entity::find_by_id(copy.playlist_id())
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .name,
            "Baseline"
        );
    }

    #[tokio::test]
    async fn keep_synced_is_unique_and_every_ordinary_mutation_is_denied_transactionally() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let snapshot = snapshot("unique-native", Some("Read only"), &["a", "b", "a"]);
        let (copy, link) = create_mirror(&manager, &snapshot).await;
        let duplicate = manager
            .create_server_playlist_mirror_from_snapshot_if_authorized(
                source_id(),
                &snapshot,
                "unused",
                || Some(()),
            )
            .await
            .expect("duplicate is typed");
        assert_eq!(duplicate, ServerPlaylistCreateOutcome::AlreadyLinked(link));
        assert_eq!(playlist::Entity::find().count(&db).await.unwrap(), 1);

        let before = entries(&db, copy.playlist_id()).await;
        assert!(manager
            .rename_playlist(copy.playlist_id(), "Denied")
            .await
            .is_err());
        assert!(manager.delete_playlist(copy.playlist_id()).await.is_err());
        let input = PlaylistEntryInput::new(
            MediaKey::new(source_id(), TrackId::remote("extra").unwrap()),
            "",
            "",
            "",
            None,
        );
        assert!(manager
            .add_entries(copy.playlist_id(), &[input])
            .await
            .is_err());
        assert!(manager
            .remove_entries(copy.playlist_id(), &[before[0].id.clone()])
            .await
            .is_err());
        assert!(manager
            .reorder_entries(
                copy.playlist_id(),
                &before
                    .iter()
                    .rev()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>(),
            )
            .await
            .is_err());
        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: Vec::new(),
            limit: None,
            sort_order: Vec::new(),
        };
        assert!(manager
            .set_smart_rules(copy.playlist_id(), &rules)
            .await
            .is_err());
        assert_eq!(entries(&db, copy.playlist_id()).await, before);
    }

    #[tokio::test]
    async fn linked_playlist_listing_is_source_isolated_and_deterministically_ordered() {
        let db = database().await;
        let manager = PlaylistManager::new(db);
        let first = snapshot("list-first", Some("First"), &["a"]);
        let second = snapshot("list-second", Some("Second"), &["b"]);
        let (first_copy, _) = create_mirror(&manager, &first).await;
        let (second_copy, _) = create_mirror(&manager, &second).await;

        let other_source: SourceId = "22222222-2222-4222-8222-222222222222"
            .parse()
            .expect("second remote source ID");
        assert!(matches!(
            manager
                .create_server_playlist_mirror_from_snapshot_if_authorized(
                    other_source,
                    &first,
                    "unused",
                    || Some(()),
                )
                .await
                .unwrap(),
            ServerPlaylistCreateOutcome::Committed { .. }
        ));

        let listed = manager
            .list_server_playlist_links(source_id())
            .await
            .unwrap();
        let listed_ids: Vec<&str> = listed
            .iter()
            .map(|link| link.playlist_id.as_str())
            .collect();
        let mut expected_ids = vec![first_copy.playlist_id(), second_copy.playlist_id()];
        expected_ids.sort_unstable();
        assert_eq!(listed_ids, expected_ids);
        assert!(listed.iter().all(|link| link.source_id == source_id()));

        let other = manager
            .list_server_playlist_links(other_source)
            .await
            .unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].source_id, other_source);
        assert_eq!(&other[0].native_playlist_id, first.native_id());
    }

    #[tokio::test]
    async fn revision_ticket_prevents_late_older_pull_and_name_only_pull_preserves_occurrence_ids()
    {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let initial = snapshot("revision-native", Some("Initial"), &["a", "b", "a"]);
        let (copy, _) = create_mirror(&manager, &initial).await;
        let first = preparation(&manager, copy.playlist_id()).await;
        let older_ticket = first.ticket().clone();
        let newer_ticket = first.ticket().clone();
        let newer = snapshot("revision-native", Some("Newer"), &["new"]);
        assert!(matches!(
            manager
                .apply_server_playlist_snapshot_if_authorized(
                    newer_ticket,
                    source_id(),
                    &newer,
                    ServerPlaylistPullPolicy::RefuseDrift,
                    || Some(()),
                )
                .await
                .unwrap(),
            ServerPlaylistPullOutcome::Applied { .. }
        ));
        let older = snapshot("revision-native", Some("Older"), &["old"]);
        assert_eq!(
            manager
                .apply_server_playlist_snapshot_if_authorized(
                    older_ticket,
                    source_id(),
                    &older,
                    ServerPlaylistPullPolicy::RefuseDrift,
                    || Some(()),
                )
                .await
                .unwrap(),
            ServerPlaylistPullOutcome::Superseded
        );
        assert_eq!(
            entries(&db, copy.playlist_id()).await[0]
                .track_id
                .as_deref(),
            Some("new")
        );

        let before_ids: Vec<String> = entries(&db, copy.playlist_id())
            .await
            .into_iter()
            .map(|row| row.id)
            .collect();
        let ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        let renamed = snapshot("revision-native", Some("Name only"), &["new"]);
        assert!(matches!(
            manager
                .apply_server_playlist_snapshot_if_authorized(
                    ticket,
                    source_id(),
                    &renamed,
                    ServerPlaylistPullPolicy::RefuseDrift,
                    || Some(()),
                )
                .await
                .unwrap(),
            ServerPlaylistPullOutcome::Applied { .. }
        ));
        let after_ids: Vec<String> = entries(&db, copy.playlist_id())
            .await
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(after_ids, before_ids);
    }

    #[tokio::test]
    async fn drift_conflicts_force_replace_restores_nameless_baseline_and_rejection_retains_state()
    {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let initial = snapshot("drift-native", Some("Baseline"), &["a", "b"]);
        let (copy, original_link) = create_mirror(&manager, &initial).await;
        direct_rename(&db, copy.playlist_id(), "Local drift secret").await;
        let ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        let incoming = snapshot("drift-native", None, &["server-new"]);
        let conflict = manager
            .apply_server_playlist_snapshot_if_authorized(
                ticket,
                source_id(),
                &incoming,
                ServerPlaylistPullPolicy::RefuseDrift,
                || Some(()),
            )
            .await
            .unwrap();
        let ServerPlaylistPullOutcome::Conflict(conflict_link) = conflict else {
            panic!("local drift must conflict")
        };
        assert_eq!(
            conflict_link.local_state,
            ServerPlaylistLocalState::Conflict
        );
        assert_eq!(
            conflict_link.membership_digest,
            original_link.membership_digest
        );
        assert_eq!(
            conflict_link.last_success_at_ms,
            original_link.last_success_at_ms
        );
        assert_eq!(entries(&db, copy.playlist_id()).await.len(), 2);

        let before_rejection = conflict_link.clone();
        let rejected_ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        assert_eq!(
            manager
                .apply_server_playlist_snapshot_if_authorized(
                    rejected_ticket,
                    source_id(),
                    &incoming,
                    ServerPlaylistPullPolicy::ReplaceLocal,
                    Option::<()>::default,
                )
                .await
                .unwrap(),
            ServerPlaylistPullOutcome::Rejected
        );
        assert_eq!(
            manager
                .get_server_playlist_link(copy.playlist_id())
                .await
                .unwrap(),
            Some(before_rejection)
        );
        assert_eq!(entries(&db, copy.playlist_id()).await.len(), 2);

        let replace_ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        assert!(matches!(
            manager
                .apply_server_playlist_snapshot_if_authorized(
                    replace_ticket,
                    source_id(),
                    &incoming,
                    ServerPlaylistPullPolicy::ReplaceLocal,
                    || Some(()),
                )
                .await
                .unwrap(),
            ServerPlaylistPullOutcome::Applied { .. }
        ));
        assert_eq!(
            playlist::Entity::find_by_id(copy.playlist_id())
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .name,
            "Baseline"
        );
        assert_eq!(
            entries(&db, copy.playlist_id()).await[0]
                .track_id
                .as_deref(),
            Some("server-new")
        );
    }

    #[tokio::test]
    async fn complete_list_missing_keeps_snapshot_and_records_conflict_orthogonally() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let initial = snapshot("missing-native", Some("Baseline"), &["a", "b"]);
        let (copy, baseline) = create_mirror(&manager, &initial).await;
        let ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        direct_rename(&db, copy.playlist_id(), "Drift while listing").await;
        assert_eq!(
            manager
                .mark_server_playlist_missing_identity_if_authorized(
                    ticket.clone(),
                    source_id(),
                    initial.native_id(),
                    Option::<()>::default,
                )
                .await
                .unwrap(),
            ServerPlaylistMissingOutcome::Rejected
        );
        assert_eq!(
            manager
                .get_server_playlist_link(copy.playlist_id())
                .await
                .unwrap(),
            Some(baseline.clone())
        );
        assert_eq!(entries(&db, copy.playlist_id()).await.len(), 2);
        let outcome = manager
            .mark_server_playlist_missing_identity_if_authorized(
                ticket,
                source_id(),
                initial.native_id(),
                || Some(()),
            )
            .await
            .unwrap();
        let ServerPlaylistMissingOutcome::Marked(missing) = outcome else {
            panic!("successful absence must be marked")
        };
        assert_eq!(missing.local_state, ServerPlaylistLocalState::Conflict);
        assert_eq!(missing.remote_state, ServerPlaylistRemoteState::Missing);
        assert_eq!(missing.membership_digest, baseline.membership_digest);
        assert_eq!(missing.last_success_at_ms, baseline.last_success_at_ms);
        assert_eq!(entries(&db, copy.playlist_id()).await.len(), 2);
        assert_eq!(
            playlist::Entity::find_by_id(copy.playlist_id())
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .name,
            "Drift while listing"
        );
    }

    #[tokio::test]
    async fn unlink_and_explicit_remove_are_revision_checked_recovery_paths() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let first = snapshot("unlink-native", Some("Unlink"), &["a", "a"]);
        let (copy, _) = create_mirror(&manager, &first).await;
        let ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        let stale = ticket.clone();
        assert!(matches!(
            manager.unlink_server_playlist(ticket).await.unwrap(),
            ServerPlaylistUnlinkOutcome::Unlinked(_)
        ));
        assert_eq!(
            manager.unlink_server_playlist(stale).await.unwrap(),
            ServerPlaylistUnlinkOutcome::Superseded
        );
        assert_eq!(entries(&db, copy.playlist_id()).await.len(), 2);
        manager
            .rename_playlist(copy.playlist_id(), "Now editable")
            .await
            .expect("unlink restores ordinary edits");

        let second = snapshot("remove-native", Some("Remove"), &["x"]);
        let (remove_copy, _) = create_mirror(&manager, &second).await;
        let remove_ticket = preparation(&manager, remove_copy.playlist_id())
            .await
            .ticket()
            .clone();
        assert_eq!(
            manager
                .remove_local_server_playlist(remove_ticket)
                .await
                .unwrap(),
            ServerPlaylistRemoveOutcome::Removed
        );
        assert!(playlist::Entity::find_by_id(remove_copy.playlist_id())
            .one(&db)
            .await
            .unwrap()
            .is_none());
        assert!(entries(&db, remove_copy.playlist_id()).await.is_empty());
    }

    #[tokio::test]
    async fn prepare_allows_explicit_recovery_from_a_corrupt_smart_link() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let snapshot = snapshot("smart-drift", Some("Was regular"), &["a"]);
        let (copy, _) = create_mirror(&manager, &snapshot).await;
        let model = playlist::Entity::find_by_id(copy.playlist_id())
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut active: playlist::ActiveModel = model.into();
        active.is_smart = Set(true);
        active.update(&db).await.unwrap();

        let ticket = preparation(&manager, copy.playlist_id())
            .await
            .ticket()
            .clone();
        assert!(matches!(
            manager.unlink_server_playlist(ticket).await.unwrap(),
            ServerPlaylistUnlinkOutcome::Unlinked(_)
        ));
    }

    #[tokio::test]
    async fn typed_diagnostics_redact_server_controlled_content() {
        let db = database().await;
        let manager = PlaylistManager::new(db);
        let secret = "secret-native-name-track";
        let snapshot = snapshot(secret, Some(secret), &[secret]);
        let (copy, link) = create_mirror(&manager, &snapshot).await;
        let prepared = preparation(&manager, copy.playlist_id()).await;
        let create = ServerPlaylistCreateOutcome::AlreadyLinked(link.clone());
        for diagnostic in [
            format!("{copy:?}"),
            format!("{link:?}"),
            format!("{:?}", prepared.ticket()),
            format!("{prepared:?}"),
            format!("{create:?}"),
        ] {
            assert!(
                !diagnostic.contains(secret),
                "diagnostic leaked: {diagnostic}"
            );
        }
    }

    #[tokio::test]
    async fn reconciliation_skips_even_directly_corrupted_linked_local_occurrences() {
        let db = database().await;
        let manager = PlaylistManager::new(db.clone());
        let snapshot = snapshot("reconcile-native", Some("Mirror"), &["remote"]);
        let (copy, _) = create_mirror(&manager, &snapshot).await;
        playlist_entry::ActiveModel {
            id: Set("linked-local-orphan".to_string()),
            playlist_id: Set(copy.playlist_id().to_string()),
            position: Set(1),
            source_id: Set(SourceId::local().to_string()),
            track_id: Set(None),
            local_track_id: Set(None),
            match_title: Set("match".to_string()),
            match_artist: Set("artist".to_string()),
            match_album: Set(String::new()),
            match_duration_secs: Set(None),
            match_file_path: Set(None),
        }
        .insert(&db)
        .await
        .expect("install older-binary drift");
        track::ActiveModel {
            id: Set("local-match".to_string()),
            file_path: Set("/music/match.flac".to_string()),
            title: Set("match".to_string()),
            artist_name: Set("artist".to_string()),
            album_artist_name: Set(None),
            album_title: Set(String::new()),
            genre: Set(None),
            composer: Set(None),
            year: Set(None),
            track_number: Set(None),
            disc_number: Set(None),
            duration_secs: Set(None),
            bitrate_kbps: Set(None),
            sample_rate_hz: Set(None),
            format: Set(None),
            play_count: Set(0),
            last_played_at_ms: Set(None),
            rating: Set(None),
            date_added: Set("2026-07-19T00:00:00Z".to_string()),
            date_modified: Set("2026-07-19T00:00:00Z".to_string()),
            file_size_bytes: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert local match");

        assert_eq!(manager.reconcile_all().await.unwrap(), 0);
        assert!(playlist_entry::Entity::find_by_id("linked-local-orphan")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .local_track_id
            .is_none());
    }
}
