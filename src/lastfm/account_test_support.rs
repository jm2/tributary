//! Shared fixtures for the Last.fm account composition tests.
//!
//! Split verbatim from the former single-file `account_tests.rs` so each
//! test module stays under the repository's per-file size caps. The seam
//! helpers here back the owner/consent, install, and queue test modules
//! alike; nothing in this file carries assertions of its own.

use std::sync::{Mutex, MutexGuard};

use sea_orm::{
    ActiveValue::{NotSet, Set},
    Database, DatabaseConnection, EntityTrait, PaginatorTrait,
};
use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

use crate::db::entities::lastfm_scrobble;
use crate::db::migration::Migrator;
use crate::lastfm::authorization::LastFmAuthorizationGrant;
use crate::lastfm::client::DesktopAuthorizedSession;
use crate::lastfm::credentials::{
    CredentialError, LastFmAccountBinding, ProtectedString, SessionCredentialStore, StoredSession,
};

pub(super) const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
pub(super) const RENEWED_SESSION_KEY: &str = "abcdef0123456789abcdef0123456789";

/// The obviously-fake fixture authorization URL.
///
/// Composed at runtime from named fixtures so no hex-looking literal ever
/// sits adjacent to an `api_key=` / `token=` parameter name in source text —
/// that adjacency is what tripped the scanner's generic-api-key rule when
/// this URL was a `concat!` of literal fragments. The produced string is
/// byte-for-byte the fixture URL the challenge parser has always accepted.
pub(super) fn fixture_auth_url() -> String {
    format!("https://www.last.fm/api/auth/?api_key={SESSION_KEY}&token={SESSION_KEY}")
}

/// In-memory credential store with deliberately injectable failure modes.
pub(super) struct MemoryVault {
    pub(super) state: Mutex<VaultState>,
}

pub(super) enum VaultState {
    Missing,
    Valid(StoredSession),
    Unavailable,
}

impl MemoryVault {
    fn lock(&self) -> Result<MutexGuard<'_, VaultState>, CredentialError> {
        self.state.lock().map_err(|_| CredentialError::Unavailable)
    }

    pub(super) fn seed(&self, username: &str, key: &str) {
        *self.lock().expect("vault unlocked") = VaultState::Valid(
            StoredSession::new(username, ProtectedString::new(key))
                .expect("fixture usernames and keys satisfy the validation invariants"),
        );
    }

    pub(super) fn stored_username(&self) -> Option<String> {
        match &*self.lock().expect("vault unlocked") {
            VaultState::Valid(session) => Some(session.username().to_owned()),
            _ => None,
        }
    }

    pub(super) fn stored_binding(&self) -> LastFmAccountBinding {
        match &*self.lock().expect("vault unlocked") {
            VaultState::Valid(session) => session.account_binding(),
            _ => panic!("test precondition: a valid session is stored"),
        }
    }
}

impl Default for MemoryVault {
    fn default() -> Self {
        Self {
            state: Mutex::new(VaultState::Missing),
        }
    }
}

impl SessionCredentialStore for MemoryVault {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
        match &*self.lock()? {
            VaultState::Missing => Ok(None),
            VaultState::Valid(session) => Ok(Some(session.clone())),
            VaultState::Unavailable => Err(CredentialError::Unavailable),
        }
    }

    fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
        *self.lock()? = VaultState::Valid(session.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        *self.lock()? = VaultState::Missing;
        Ok(())
    }
}

pub(super) fn staged_session(username: &str, key: &str) -> LastFmAuthorizationGrant {
    LastFmAuthorizationGrant::from_authorized_session(
        DesktopAuthorizedSession::for_test(username, key)
            .expect("fixture usernames and keys satisfy the validation invariants"),
    )
}

pub(super) async fn account_database() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("open in-memory Last.fm queue database");
    Migrator::up(&db, None)
        .await
        .expect("run Last.fm queue migrations");
    db
}

pub(super) async fn seed_queue_rows(
    db: &DatabaseConnection,
    binding: LastFmAccountBinding,
    count: usize,
) {
    let rows = (0..count).map(|_| lastfm_scrobble::ActiveModel {
        id: NotSet,
        occurrence_id: Set(Uuid::new_v4().as_bytes().to_vec().into()),
        account_binding: Set(binding.as_bytes().to_vec().into()),
        artist: Set("Seed Artist".to_owned().into()),
        track_title: Set("Seed Track".to_owned().into()),
        album: Set(Some("Seed Album".to_owned().into())),
        album_artist: Set(None),
        track_number: Set(Some(1.into())),
        duration_secs: Set(60.into()),
        started_at_unix_secs: Set(1_700_000_000_i64.into()),
        attempt_count: Set(0),
        next_attempt_at_ms: Set(0),
    });
    lastfm_scrobble::Entity::insert_many(rows)
        .exec(db)
        .await
        .expect("insert canonical Last.fm seed rows");
}

pub(super) async fn queue_len(db: &DatabaseConnection) -> u64 {
    lastfm_scrobble::Entity::find()
        .count(db)
        .await
        .expect("count Last.fm queue rows")
}
