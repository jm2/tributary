//! Durable Last.fm policy generation.
//!
//! One immutable snapshot records the user's explicit disclosure consent,
//! integration enablement, and exact per-source opt-in set. Every consented
//! decision survives restart in the `lastfm_policy` singleton installed by
//! migration 20; the row is absent until the user makes a first decision, so
//! fresh profiles load the closed default and the feature stays fail-closed.
//!
//! The generation counter is monotonic: every committed change produces a new
//! immutable snapshot, so queue capture and activation issuance can freeze one
//! exact policy observation without holding database locks. Loading strictly
//! validates every stored field and fails closed on malformed records instead
//! of guessing. The store deliberately contains no credentials, listening
//! history, or diagnostics text, and `Debug` implementations redact the
//! user's decisions.

use std::collections::HashSet;
use std::fmt;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use thiserror::Error;
use uuid::Uuid;

use crate::architecture::SourceId;

/// The migration-20 policy singleton table.
const POLICY_TABLE: &str = "lastfm_policy";

/// Fixed singleton primary key shared with the delivery-pause table.
const POLICY_SLOT: i64 = 1;

/// Maximum number of remote sources that may be simultaneously opted in.
///
/// This mirrors the activation factory's bound in `production.rs`; one shared
/// constant keeps the persisted policy and the runtime admission check from
/// drifting apart.
pub const LASTFM_MAX_ENABLED_REMOTE_SOURCES: usize = 256;

/// Maximum length of a persisted consent locale tag. BCP-47 language tags
/// are bounded at 35 ASCII characters.
pub const LASTFM_CONSENT_LOCALE_MAX_BYTES: usize = 35;

/// Serialized length of one canonical hyphenated source UUID.
const ENABLED_SOURCE_ENTRY_BYTES: usize = 36;

/// Maximum serialized length of the opted-in source list:
/// 256 canonical UUIDs joined by 255 commas.
pub const LASTFM_ENABLED_SOURCES_MAX_BYTES: usize = LASTFM_MAX_ENABLED_REMOTE_SOURCES
    * ENABLED_SOURCE_ENTRY_BYTES
    + (LASTFM_MAX_ENABLED_REMOTE_SOURCES - 1);

/// Fixed, content-free policy store failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum LastFmPolicyStoreError {
    /// The database refused the read or write.
    #[error("Last.fm policy storage failed")]
    Storage,
    /// The persisted record failed exact revalidation; the feature must stay
    /// closed rather than guess at a corrupt decision.
    #[error("Last.fm policy record is malformed")]
    Malformed,
    /// The snapshot the caller observed is no longer current.
    #[error("Last.fm policy changed concurrently")]
    Conflict,
    /// The requested update violates policy invariants.
    #[error("Last.fm policy update is invalid")]
    InvalidUpdate,
}

/// One accepted localized disclosure.
///
/// The locale names the exact shipped disclosure language the user accepted
/// and the revision identifies the disclosure text inside that locale, so a
/// future disclosure change can require fresh consent without reinterpreting
/// an old decision.
#[derive(Clone, PartialEq, Eq)]
pub struct LastFmConsentRecord {
    locale: String,
    disclosure_revision: u32,
}

impl LastFmConsentRecord {
    /// Validate and construct one consent record.
    ///
    /// The locale must be a nonempty ASCII tag of at most 35 characters drawn
    /// from the BCP-47 alphabet (letters, digits, and hyphens). The revision
    /// must be positive.
    pub fn try_new(locale: &str, disclosure_revision: u32) -> Result<Self, LastFmPolicyStoreError> {
        if locale.is_empty()
            || locale.len() > LASTFM_CONSENT_LOCALE_MAX_BYTES
            || !locale
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(LastFmPolicyStoreError::InvalidUpdate);
        }
        if disclosure_revision == 0 {
            return Err(LastFmPolicyStoreError::InvalidUpdate);
        }
        Ok(Self {
            locale: locale.to_owned(),
            disclosure_revision,
        })
    }

    /// The accepted disclosure language tag.
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// The accepted disclosure revision inside that locale.
    pub const fn disclosure_revision(&self) -> u32 {
        self.disclosure_revision
    }
}

impl fmt::Debug for LastFmConsentRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmConsentRecord(<redacted>)")
    }
}

/// One immutable Last.fm policy generation.
///
/// Construction is restricted to validated loads and validated commits, so no
/// caller can assemble an enabled policy that bypasses consent invariants.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LastFmPolicyGeneration {
    generation: u64,
    consent: Option<LastFmConsentRecord>,
    enabled: bool,
    enabled_remote_sources: HashSet<SourceId>,
}

impl LastFmPolicyGeneration {
    /// Monotonic generation identity. Zero identifies the closed default that
    /// was never persisted.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The accepted disclosure, if any.
    pub const fn consent(&self) -> Option<&LastFmConsentRecord> {
        self.consent.as_ref()
    }

    /// Whether the integration is enabled. Enablement requires consent.
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The exact remote-source opt-in set.
    pub fn enabled_remote_sources(&self) -> &HashSet<SourceId> {
        &self.enabled_remote_sources
    }

    /// Whether explicit consent and enablement are both present.
    pub fn consented_and_enabled(&self) -> bool {
        self.consent.is_some() && self.enabled
    }

    /// The source set queue capture must observe.
    ///
    /// Without consent or enablement the set is empty, so capture can pass the
    /// value straight to the registry's admission policy without a branch.
    pub fn queue_capture_remote_sources(&self) -> &HashSet<SourceId> {
        if self.consented_and_enabled() {
            &self.enabled_remote_sources
        } else {
            empty_remote_sources()
        }
    }

    /// The activation basis for the application owner's activation factory.
    ///
    /// `None` means no activation authority may be issued from this
    /// generation; `Some` is the exact remote-source set the activation will
    /// freeze.
    pub fn activation_remote_sources(&self) -> Option<&HashSet<SourceId>> {
        if self.consented_and_enabled() {
            Some(&self.enabled_remote_sources)
        } else {
            None
        }
    }
}

impl fmt::Debug for LastFmPolicyGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPolicyGeneration(<redacted>)")
    }
}

static EMPTY_REMOTE_SOURCES: std::sync::OnceLock<HashSet<SourceId>> = std::sync::OnceLock::new();

fn empty_remote_sources() -> &'static HashSet<SourceId> {
    EMPTY_REMOTE_SOURCES.get_or_init(HashSet::new)
}

/// One complete, validated desired policy state.
///
/// Updates are whole-snapshot: the caller describes the exact desired next
/// state and the store publishes it as the successor generation, which keeps
/// every invariant (consent before enablement, bounded opt-in set) enforced
/// in one place.
#[derive(Clone, Default)]
pub struct LastFmPolicyUpdate {
    pub consent: Option<LastFmConsentRecord>,
    pub enabled: bool,
    pub enabled_remote_sources: HashSet<SourceId>,
}

impl LastFmPolicyUpdate {
    fn validate(&self) -> Result<(), LastFmPolicyStoreError> {
        if self.enabled && self.consent.is_none() {
            return Err(LastFmPolicyStoreError::InvalidUpdate);
        }
        if self.enabled_remote_sources.len() > LASTFM_MAX_ENABLED_REMOTE_SOURCES {
            return Err(LastFmPolicyStoreError::InvalidUpdate);
        }
        if self
            .enabled_remote_sources
            .iter()
            .any(|source_id| source_id.is_reserved_remote())
        {
            return Err(LastFmPolicyStoreError::InvalidUpdate);
        }
        Ok(())
    }
}

/// Load the current policy generation.
///
/// A missing row is the fresh-profile closed default. Any stored value that
/// fails exact revalidation — locale, revision, enablement, generation, or
/// source-set shape — fails closed with [`LastFmPolicyStoreError::Malformed`].
pub async fn load_policy_generation<C>(
    db: &C,
) -> Result<LastFmPolicyGeneration, LastFmPolicyStoreError>
where
    C: ConnectionTrait,
{
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "SELECT policy_generation, consent_granted, consent_locale,
                        consent_disclosure_revision, enabled, enabled_sources
                 FROM {POLICY_TABLE} WHERE slot = ?"
            ),
            [POLICY_SLOT.into()],
        ))
        .await
        .map_err(|_| LastFmPolicyStoreError::Storage)?;
    let Some(row) = row else {
        return Ok(LastFmPolicyGeneration::default());
    };

    let generation = row
        .try_get::<i64>("", "policy_generation")
        .map_err(|_| LastFmPolicyStoreError::Malformed)?;
    if generation <= 0 {
        return Err(LastFmPolicyStoreError::Malformed);
    }
    let consent_granted = row
        .try_get::<i64>("", "consent_granted")
        .map_err(|_| LastFmPolicyStoreError::Malformed)?;
    let enabled = row
        .try_get::<i64>("", "enabled")
        .map_err(|_| LastFmPolicyStoreError::Malformed)?;
    if !matches!(consent_granted, 0 | 1) || !matches!(enabled, 0 | 1) {
        return Err(LastFmPolicyStoreError::Malformed);
    }
    let consent_granted = consent_granted == 1;
    let enabled = enabled == 1;

    let locale = row
        .try_get::<String>("", "consent_locale")
        .map_err(|_| LastFmPolicyStoreError::Malformed)?;
    let disclosure_revision = row
        .try_get::<i64>("", "consent_disclosure_revision")
        .map_err(|_| LastFmPolicyStoreError::Malformed)?;
    let consent = if consent_granted {
        let revision =
            u32::try_from(disclosure_revision).map_err(|_| LastFmPolicyStoreError::Malformed)?;
        let record = LastFmConsentRecord::try_new(&locale, revision)
            .map_err(|_| LastFmPolicyStoreError::Malformed)?;
        Some(record)
    } else {
        if !locale.is_empty() || disclosure_revision != 0 {
            return Err(LastFmPolicyStoreError::Malformed);
        }
        None
    };

    let enabled_sources = row
        .try_get::<String>("", "enabled_sources")
        .map_err(|_| LastFmPolicyStoreError::Malformed)?;
    let enabled_remote_sources = parse_enabled_sources(&enabled_sources)?;

    if enabled && !consent_granted {
        return Err(LastFmPolicyStoreError::Malformed);
    }

    Ok(LastFmPolicyGeneration {
        generation: generation as u64,
        consent,
        enabled,
        enabled_remote_sources,
    })
}

/// Commit one complete policy update as the successor generation.
///
/// The write is a compare-and-swap against `observed_generation`: zero
/// observes the unpersisted closed default, any other value the exact
/// generation the caller loaded. The transaction revalidates the current row
/// before replacing it, so concurrent writers cannot silently drop each
/// other's decisions.
pub async fn commit_policy_update(
    db: &DatabaseConnection,
    observed_generation: u64,
    update: LastFmPolicyUpdate,
) -> Result<LastFmPolicyGeneration, LastFmPolicyStoreError> {
    update.validate()?;
    let transaction = db
        .begin()
        .await
        .map_err(|_| LastFmPolicyStoreError::Storage)?;

    let current = load_policy_generation(&transaction).await?;
    if current.generation() != observed_generation {
        return Err(LastFmPolicyStoreError::Conflict);
    }
    // The generation counter is bounded by u64; an exhausted counter must fail
    // closed instead of wrapping into a replayable identity.
    let next_generation = current
        .generation()
        .checked_add(1)
        .ok_or(LastFmPolicyStoreError::Storage)?;

    let (consent_granted, consent_locale, consent_revision) = match &update.consent {
        Some(record) => (1_i64, record.locale.as_str(), record.disclosure_revision),
        None => (0_i64, "", 0_u32),
    };
    let enabled = i64::from(update.enabled);
    let enabled_sources = serialize_enabled_sources(&update.enabled_remote_sources);

    transaction
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "INSERT INTO {POLICY_TABLE}
                     (slot, policy_generation, consent_granted, consent_locale,
                      consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(slot) DO UPDATE SET
                     policy_generation = excluded.policy_generation,
                     consent_granted = excluded.consent_granted,
                     consent_locale = excluded.consent_locale,
                     consent_disclosure_revision = excluded.consent_disclosure_revision,
                     enabled = excluded.enabled,
                     enabled_sources = excluded.enabled_sources"
            ),
            [
                POLICY_SLOT.into(),
                (next_generation as i64).into(),
                consent_granted.into(),
                consent_locale.into(),
                consent_revision.into(),
                enabled.into(),
                enabled_sources.into(),
            ],
        ))
        .await
        .map_err(|_| LastFmPolicyStoreError::Storage)?;

    transaction
        .commit()
        .await
        .map_err(|_| LastFmPolicyStoreError::Storage)?;

    Ok(LastFmPolicyGeneration {
        generation: next_generation,
        consent: update.consent,
        enabled: update.enabled,
        enabled_remote_sources: update.enabled_remote_sources,
    })
}

/// Canonical serialized form: sorted lowercase canonical hyphenated UUIDs
/// joined by commas. Sorting keeps the persisted bytes a pure function of the
/// set, so byte equality implies semantic equality.
fn serialize_enabled_sources(sources: &HashSet<SourceId>) -> String {
    let mut identifiers: Vec<String> = sources.iter().map(|id| id.as_uuid().to_string()).collect();
    identifiers.sort();
    identifiers.join(",")
}

/// Strictly revalidate the persisted source list.
fn parse_enabled_sources(serialized: &str) -> Result<HashSet<SourceId>, LastFmPolicyStoreError> {
    if serialized.is_empty() {
        return Ok(HashSet::new());
    }
    if serialized.len() > LASTFM_ENABLED_SOURCES_MAX_BYTES {
        return Err(LastFmPolicyStoreError::Malformed);
    }
    let mut sources = HashSet::new();
    for entry in serialized.split(',') {
        let uuid = Uuid::parse_str(entry).map_err(|_| LastFmPolicyStoreError::Malformed)?;
        let source_id = SourceId::from_uuid(uuid);
        if source_id.is_reserved_remote() {
            return Err(LastFmPolicyStoreError::Malformed);
        }
        if !sources.insert(source_id) {
            return Err(LastFmPolicyStoreError::Malformed);
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    use super::*;

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::migration::Migrator::up(&db, None).await.unwrap();
        db
    }

    fn sample_source(seed: u64) -> SourceId {
        SourceId::from_uuid(Uuid::from_u128(
            0x0000_0000_0000_4000_8000_0000_0000_0000 | ((seed & 0xffff_ffff) as u128) << 8,
        ))
    }

    fn consent(locale: &str) -> LastFmConsentRecord {
        LastFmConsentRecord::try_new(locale, 1).unwrap()
    }

    fn update(
        consent: Option<LastFmConsentRecord>,
        enabled: bool,
        sources: HashSet<SourceId>,
    ) -> LastFmPolicyUpdate {
        LastFmPolicyUpdate {
            consent,
            enabled,
            enabled_remote_sources: sources,
        }
    }

    #[tokio::test]
    async fn fresh_profile_loads_the_closed_default_without_a_row() {
        let db = database().await;
        let policy = load_policy_generation(&db).await.unwrap();
        assert_eq!(policy.generation(), 0);
        assert!(!policy.is_enabled());
        assert!(policy.consent().is_none());
        assert!(policy.enabled_remote_sources().is_empty());
        assert!(!policy.consented_and_enabled());
        assert!(policy.activation_remote_sources().is_none());
        assert!(policy.queue_capture_remote_sources().is_empty());
    }

    #[tokio::test]
    async fn consented_enablement_feeds_capture_and_activation() {
        let db = database().await;
        let mut sources = HashSet::new();
        sources.insert(sample_source(1));

        let committed = commit_policy_update(
            &db,
            0,
            update(Some(consent("en-US")), true, sources.clone()),
        )
        .await
        .unwrap();
        assert_eq!(committed.generation(), 1);
        assert_eq!(committed.consent(), Some(&consent("en-US")));
        assert!(committed.is_enabled());
        assert_eq!(committed.queue_capture_remote_sources(), &sources);
        assert_eq!(committed.activation_remote_sources(), Some(&sources));

        let loaded = load_policy_generation(&db).await.unwrap();
        assert_eq!(loaded.generation(), 1);
        assert_eq!(loaded, committed);
        assert_eq!(loaded.queue_capture_remote_sources(), &sources);
        assert_eq!(loaded.activation_remote_sources(), Some(&sources));
    }

    #[tokio::test]
    async fn enablement_without_consent_is_rejected_everywhere() {
        let db = database().await;
        assert_eq!(
            commit_policy_update(&db, 0, update(None, true, HashSet::new()))
                .await
                .unwrap_err(),
            LastFmPolicyStoreError::InvalidUpdate
        );
        assert_eq!(load_policy_generation(&db).await.unwrap().generation(), 0);

        // The database CHECK constraint enforces the same invariant against
        // out-of-band writes.
        let error = db
            .execute_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "INSERT INTO {POLICY_TABLE}
                         (slot, policy_generation, consent_granted, consent_locale,
                          consent_disclosure_revision, enabled, enabled_sources)
                     VALUES (1, 1, 0, '', 0, 1, '')"
                ),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("CHECK"), "{error}");
        assert_eq!(load_policy_generation(&db).await.unwrap().generation(), 0);
    }

    #[tokio::test]
    async fn stale_observed_generation_conflicts_and_never_overwrites() {
        let db = database().await;
        let first =
            commit_policy_update(&db, 0, update(Some(consent("de")), false, HashSet::new()))
                .await
                .unwrap();
        let second = commit_policy_update(
            &db,
            first.generation(),
            update(first.consent().cloned(), true, HashSet::new()),
        )
        .await
        .unwrap();
        assert_eq!(second.generation(), 2);

        // The stale snapshot (generation 1) must not overwrite generation 2.
        assert_eq!(
            commit_policy_update(&db, 1, update(Some(consent("fr")), false, HashSet::new()))
                .await
                .unwrap_err(),
            LastFmPolicyStoreError::Conflict
        );
        assert_eq!(load_policy_generation(&db).await.unwrap(), second);

        // A stale writer observing "no row" cannot clobber an existing row.
        assert_eq!(
            commit_policy_update(&db, 0, update(Some(consent("es")), false, HashSet::new()))
                .await
                .unwrap_err(),
            LastFmPolicyStoreError::Conflict
        );
        assert_eq!(load_policy_generation(&db).await.unwrap(), second);
    }

    #[tokio::test]
    async fn disabling_clears_capture_and_activation_even_with_consent_retained() {
        let db = database().await;
        let mut sources = HashSet::new();
        sources.insert(sample_source(2));
        let enabled_gen = commit_policy_update(&db, 0, update(Some(consent("en")), true, sources))
            .await
            .unwrap();
        assert!(!enabled_gen.queue_capture_remote_sources().is_empty());

        let disabled = commit_policy_update(
            &db,
            enabled_gen.generation(),
            update(enabled_gen.consent().cloned(), false, HashSet::new()),
        )
        .await
        .unwrap();
        assert_eq!(disabled.generation(), 2);
        assert!(disabled.consent().is_some());
        assert!(!disabled.is_enabled());
        assert!(disabled.queue_capture_remote_sources().is_empty());
        assert!(disabled.activation_remote_sources().is_none());

        assert_eq!(load_policy_generation(&db).await.unwrap(), disabled);
    }

    #[tokio::test]
    async fn reserved_nil_and_oversized_source_sets_are_rejected() {
        let db = database().await;
        for source in [
            SourceId::local(),
            SourceId::radio_browser(),
            SourceId::from_uuid(Uuid::nil()),
        ] {
            let mut sources = HashSet::new();
            sources.insert(source);
            assert_eq!(
                commit_policy_update(&db, 0, update(Some(consent("en")), true, sources))
                    .await
                    .unwrap_err(),
                LastFmPolicyStoreError::InvalidUpdate
            );
        }

        let oversized: HashSet<SourceId> = (0..=LASTFM_MAX_ENABLED_REMOTE_SOURCES as u64)
            .map(sample_source)
            .collect();
        assert_eq!(
            commit_policy_update(&db, 0, update(Some(consent("en")), true, oversized))
                .await
                .unwrap_err(),
            LastFmPolicyStoreError::InvalidUpdate
        );
    }

    #[tokio::test]
    async fn malformed_consent_payloads_are_rejected_before_persistence() {
        assert!(LastFmConsentRecord::try_new("", 1).is_err());
        assert!(LastFmConsentRecord::try_new(&"x".repeat(36), 1).is_err());
        assert!(LastFmConsentRecord::try_new("en_US", 1).is_err());
        assert!(LastFmConsentRecord::try_new("dé", 1).is_err());
        assert!(LastFmConsentRecord::try_new("en", 0).is_err());
        assert!(LastFmConsentRecord::try_new("en", u32::MAX).is_ok());
    }

    #[tokio::test]
    async fn corrupt_stored_rows_fail_closed_as_malformed() {
        let db = database().await;
        let mut sources = HashSet::new();
        sources.insert(sample_source(3));
        let committed = commit_policy_update(&db, 0, update(Some(consent("en")), true, sources))
            .await
            .unwrap();

        // Storage-layer defense: CHECK constraints refuse incoherent rows
        // outright. The typed loader below remains the second line of
        // defense for anything a hostile or legacy database manages to
        // store.
        let check_rejections = [
            "UPDATE lastfm_policy SET consent_granted = 0, consent_locale = '',
                    consent_disclosure_revision = 0
             WHERE slot = 1",
            "UPDATE lastfm_policy SET consent_locale = 'd' || char(233) WHERE slot = 1",
            "UPDATE lastfm_policy SET consent_disclosure_revision = 0 WHERE slot = 1",
            "UPDATE lastfm_policy SET policy_generation = 0 WHERE slot = 1",
            "UPDATE lastfm_policy SET enabled = 2 WHERE slot = 1",
        ];
        for corruption in check_rejections {
            let outcome = db.execute_unprepared(corruption).await;
            assert!(outcome.is_err(), "CHECK accepted corruption: {corruption}");
            assert_eq!(
                load_policy_generation(&db).await.unwrap(),
                committed,
                "CHECK rejection must not leave the row changed: {corruption}"
            );
        }

        // Loader-layer defense: these rows satisfy every CHECK constraint
        // but must fail exact revalidation. Each corruption runs inside a
        // rolled-back transaction so the next case starts from the exact
        // committed row.
        let duplicated_sources: String = format!(
            "UPDATE lastfm_policy SET enabled_sources = '{nil}{nil}' WHERE slot = 1",
            nil = "00000000-0000-0000-0000-000000000000,"
        );
        let corruptions = [
            "UPDATE lastfm_policy SET enabled_sources = 'not-a-uuid' WHERE slot = 1",
            "UPDATE lastfm_policy SET enabled_sources = '00000000-0000-0000-0000-000000000000'
             WHERE slot = 1",
            duplicated_sources.as_str(),
            "UPDATE lastfm_policy SET enabled_sources =
                 enabled_sources || ',' || enabled_sources
             WHERE slot = 1",
        ];
        for corruption in corruptions {
            let transaction = db.begin().await.unwrap();
            transaction.execute_unprepared(corruption).await.unwrap();
            assert_eq!(
                load_policy_generation(&transaction).await.unwrap_err(),
                LastFmPolicyStoreError::Malformed,
                "corruption did not fail closed: {corruption}"
            );
            transaction.rollback().await.unwrap();
        }
        assert_eq!(load_policy_generation(&db).await.unwrap(), committed);
    }

    #[tokio::test]
    async fn serialization_is_a_pure_function_of_the_set() {
        let mut left = HashSet::new();
        left.insert(sample_source(1));
        left.insert(sample_source(2));
        let mut right = HashSet::new();
        right.insert(sample_source(2));
        right.insert(sample_source(1));
        assert_eq!(
            serialize_enabled_sources(&left),
            serialize_enabled_sources(&right)
        );
        let parsed = parse_enabled_sources(&serialize_enabled_sources(&left)).unwrap();
        assert_eq!(parsed, left);
    }

    #[tokio::test]
    async fn debug_implementations_redact_user_decisions() {
        let record = consent("en-US");
        assert_eq!(format!("{record:?}"), "LastFmConsentRecord(<redacted>)");
        let mut sources = HashSet::new();
        sources.insert(sample_source(1));
        let policy = LastFmPolicyGeneration {
            generation: 7,
            consent: Some(record),
            enabled: true,
            enabled_remote_sources: sources,
        };
        assert_eq!(format!("{policy:?}"), "LastFmPolicyGeneration(<redacted>)");
    }
}
