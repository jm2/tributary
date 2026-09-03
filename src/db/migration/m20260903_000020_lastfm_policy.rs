//! Migration: persist the durable Last.fm policy singleton.
//!
//! The singleton records the user's explicit disclosure consent, integration
//! enablement, and exact per-source opt-in list as one validated row. It
//! deliberately contains no credentials, listening history, endpoint, or
//! diagnostic text. The row is absent until the user makes a first policy
//! decision, so fresh profiles and downgraded databases keep the feature
//! fail-closed.
//!
//! Column CHECK constraints enforce the core invariants at the storage
//! boundary: a singleton slot, a positive generation, consent-gated
//! enablement, an ASCII-bounded locale tag with a positive disclosure
//! revision exactly when consent is granted, and an ASCII-bounded source
//! list. The typed policy boundary in `crate::lastfm::policy` revalidates
//! every field on load and fails closed on malformed records.

use std::fmt;

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{
    ConnectionTrait, DatabaseConnection, Statement, TransactionTrait,
};

const TABLE: &str = "lastfm_policy";
const PAUSE_TABLE: &str = "lastfm_delivery_pause";
const POLICY_SLOT: i64 = 1;
const MAX_LOCALE_BYTES: i64 = 35;
const MAX_ENABLED_SOURCES_BYTES: i64 = 256 * 36 + 255;
const MAX_DISCLOSURE_REVISION: i64 = i32::MAX as i64;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_policy(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_policy(manager, false).await
    }
}

/// Revalidate the exact policy schema on every startup.
pub(super) async fn revalidate(connection: &DatabaseConnection) -> Result<(), DbErr> {
    validate_schema(&SchemaManager::new(connection)).await
}

async fn migrate_policy(manager: &SchemaManager<'_>, create: bool) -> Result<(), DbErr> {
    let transaction = manager.get_connection().begin().await?;
    let result = {
        let manager = SchemaManager::new(&transaction);
        if create {
            create_or_validate(&manager).await
        } else {
            drop_if_lossless(&manager).await
        }
    };
    match result {
        Ok(()) => transaction.commit().await,
        Err(error) => {
            let rollback = transaction.rollback().await;
            Err(match rollback {
                Ok(()) => error,
                Err(rollback_error) => DbErr::Migration(format!(
                    "{error}; additionally failed to roll back Last.fm policy migration: {rollback_error}"
                )),
            })
        }
    }
}

async fn create_or_validate(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    // The policy records consent decisions, so it must never exist beside an
    // older database that predates the scrobble infrastructure it gates.
    match named_object_type(manager, PAUSE_TABLE).await? {
        Some(object_type) if object_type == "table" => {}
        Some(object_type) => {
            return Err(DbErr::Migration(format!(
                "{PAUSE_TABLE} must be a table, found {object_type}"
            )));
        }
        None => {
            return Err(DbErr::Migration(format!(
                "{PAUSE_TABLE} must exist before {TABLE}"
            )));
        }
    }
    match object_type(manager).await? {
        None => {
            manager
                .get_connection()
                .execute_unprepared(&canonical_table_sql())
                .await?;
        }
        Some(object_type) if object_type == "table" => {}
        Some(object_type) => {
            return Err(DbErr::Migration(format!(
                "{TABLE} must be a table, found {object_type}"
            )));
        }
    }
    validate_schema(manager).await
}

async fn drop_if_lossless(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let Some(target_type) = object_type(manager).await? else {
        return Ok(());
    };
    if target_type != "table" {
        return Err(DbErr::Migration(format!(
            "{TABLE} must be a table, found {target_type}"
        )));
    }
    validate_schema(manager).await?;
    let policy_count = manager
        .get_connection()
        .query_one_raw(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT COUNT(*) AS count FROM {TABLE}"),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("failed to inspect Last.fm policy state".to_owned()))?
        .try_get::<i64>("", "count")?;
    if policy_count != 0 {
        return Err(DbErr::Migration(format!(
            "cannot downgrade {policy_count} recorded Last.fm policy decision(s) safely"
        )));
    }
    manager
        .get_connection()
        .execute_unprepared(&format!("DROP TABLE {TABLE}"))
        .await?;
    if object_type(manager).await?.is_some() {
        return Err(DbErr::Migration(format!(
            "{TABLE} remained after downgrade"
        )));
    }
    Ok(())
}

fn canonical_table_sql() -> String {
    format!(
        "CREATE TABLE {TABLE} (
             slot INTEGER PRIMARY KEY,
             policy_generation INTEGER NOT NULL,
             consent_granted INTEGER NOT NULL,
             consent_locale TEXT NOT NULL,
             consent_disclosure_revision INTEGER NOT NULL,
             enabled INTEGER NOT NULL,
             enabled_sources TEXT NOT NULL,
             CONSTRAINT ck_lastfm_policy_slot CHECK (
                 typeof(slot) = 'integer' AND slot = {POLICY_SLOT}
             ),
             CONSTRAINT ck_lastfm_policy_generation CHECK (
                 typeof(policy_generation) = 'integer'
                 AND policy_generation BETWEEN 1 AND 9223372036854775807
             ),
             CONSTRAINT ck_lastfm_policy_consent_granted CHECK (
                 typeof(consent_granted) = 'integer' AND consent_granted IN (0, 1)
             ),
             CONSTRAINT ck_lastfm_policy_consent_locale CHECK (
                 (
                     consent_granted = 1
                     AND typeof(consent_locale) = 'text'
                     AND length(consent_locale) BETWEEN 1 AND {MAX_LOCALE_BYTES}
                     AND length(consent_locale) = length(CAST(consent_locale AS BLOB))
                 )
                 OR (consent_granted = 0 AND consent_locale = '')
             ),
             CONSTRAINT ck_lastfm_policy_disclosure_revision CHECK (
                 (
                     consent_granted = 1
                     AND typeof(consent_disclosure_revision) = 'integer'
                     AND consent_disclosure_revision BETWEEN 1 AND {MAX_DISCLOSURE_REVISION}
                 )
                 OR (consent_granted = 0 AND consent_disclosure_revision = 0)
             ),
             CONSTRAINT ck_lastfm_policy_enabled CHECK (
                 typeof(enabled) = 'integer' AND enabled IN (0, 1)
                 AND (enabled = 0 OR consent_granted = 1)
             ),
             CONSTRAINT ck_lastfm_policy_enabled_sources CHECK (
                 typeof(enabled_sources) = 'text'
                 AND length(enabled_sources) <= {MAX_ENABLED_SOURCES_BYTES}
                 AND length(enabled_sources) = length(CAST(enabled_sources AS BLOB))
             )
         )"
    )
}

async fn object_type(manager: &SchemaManager<'_>) -> Result<Option<String>, DbErr> {
    named_object_type(manager, TABLE).await
}

async fn named_object_type(
    manager: &SchemaManager<'_>,
    name: &str,
) -> Result<Option<String>, DbErr> {
    manager
        .get_connection()
        .query_one_raw(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT type FROM sqlite_master WHERE name = ?",
            [name.into()],
        ))
        .await?
        .map(|row| row.try_get("", "type"))
        .transpose()
}

type ColumnSchema = (i32, String, String, i32, Option<String>, i32);

async fn validate_schema(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let columns = manager
        .get_connection()
        .query_all_raw(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA table_info('{TABLE}')"),
        ))
        .await
        .map_err(|error| DbErr::Migration(format!("failed to inspect {TABLE}: {error}")))?
        .into_iter()
        .map(|row| {
            Ok::<ColumnSchema, DbErr>((
                row.try_get("", "cid")?,
                row.try_get("", "name")?,
                row.try_get::<String>("", "type")?.to_ascii_lowercase(),
                row.try_get("", "notnull")?,
                row.try_get("", "dflt_value")?,
                row.try_get("", "pk")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![
        (0, "slot".to_owned(), "integer".to_owned(), 0, None, 1),
        (
            1,
            "policy_generation".to_owned(),
            "integer".to_owned(),
            1,
            None,
            0,
        ),
        (
            2,
            "consent_granted".to_owned(),
            "integer".to_owned(),
            1,
            None,
            0,
        ),
        (
            3,
            "consent_locale".to_owned(),
            "text".to_owned(),
            1,
            None,
            0,
        ),
        (
            4,
            "consent_disclosure_revision".to_owned(),
            "integer".to_owned(),
            1,
            None,
            0,
        ),
        (5, "enabled".to_owned(), "integer".to_owned(), 1, None, 0),
        (
            6,
            "enabled_sources".to_owned(),
            "text".to_owned(),
            1,
            None,
            0,
        ),
    ];
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected column schema: {columns:?}"
        )));
    }

    let row = manager
        .get_connection()
        .query_one_raw(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
            [TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("{TABLE} SQL is missing")))?;
    let actual: String = row.try_get("", "sql")?;
    if canonical_sql(&actual) != canonical_sql(&canonical_table_sql()) {
        return Err(DbErr::Migration(format!(
            "{TABLE} does not have the exact canonical table definition"
        )));
    }

    for (kind, pragma) in [
        ("index", format!("PRAGMA index_list('{TABLE}')")),
        ("foreign key", format!("PRAGMA foreign_key_list('{TABLE}')")),
    ] {
        let rows = manager
            .get_connection()
            .query_all_raw(Statement::from_string(
                manager.get_database_backend(),
                pragma,
            ))
            .await?;
        if !rows.is_empty() {
            return Err(DbErr::Migration(format!(
                "{TABLE} has unexpected {kind} objects"
            )));
        }
    }
    let trigger_count = manager
        .get_connection()
        .query_one_raw(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT COUNT(*) AS count FROM sqlite_master
             WHERE type = 'trigger' AND tbl_name = ?",
            [TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("failed to inspect {TABLE} triggers")))?
        .try_get::<i64>("", "count")?;
    if trigger_count != 0 {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {trigger_count} unexpected trigger(s)"
        )));
    }
    Ok(())
}

/// Normalize harmless SQLite formatting and identifier quoting while keeping
/// string literals byte-exact.
fn canonical_sql(sql: &str) -> String {
    let mut canonical = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                canonical.push('\'');
                while let Some(literal_character) = characters.next() {
                    canonical.push(literal_character);
                    if literal_character == '\'' {
                        if characters.peek() == Some(&'\'') {
                            canonical.push(characters.next().expect("peeked quote exists"));
                        } else {
                            break;
                        }
                    }
                }
            }
            '"' => append_quoted_identifier(&mut canonical, &mut characters, '"'),
            '`' => append_quoted_identifier(&mut canonical, &mut characters, '`'),
            '[' => append_quoted_identifier(&mut canonical, &mut characters, ']'),
            character if character.is_ascii_whitespace() => {}
            character => canonical.extend(character.to_lowercase()),
        }
    }
    canonical
}

fn append_quoted_identifier<I>(
    canonical: &mut String,
    characters: &mut std::iter::Peekable<I>,
    closing_quote: char,
) where
    I: Iterator<Item = char>,
{
    while let Some(character) = characters.next() {
        if character == closing_quote {
            if characters.peek() == Some(&closing_quote) {
                canonical.extend(character.to_lowercase());
                characters.next();
            } else {
                break;
            }
        } else {
            canonical.extend(character.to_lowercase());
        }
    }
}

impl fmt::Debug for Migration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPolicyMigration")
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{Database, DatabaseConnection, DbBackend};

    use super::*;
    use crate::db::migration::Migrator;

    async fn database_through_delivery_pause() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, Some(19)).await.unwrap();
        database
    }

    async fn migrated_database() -> DatabaseConnection {
        let database = database_through_delivery_pause().await;
        Migrator::up(&database, Some(1)).await.unwrap();
        database
    }

    async fn target_exists(database: &DatabaseConnection) -> bool {
        object_type(&SchemaManager::new(database)).await.unwrap() == Some("table".to_owned())
    }

    #[tokio::test]
    async fn existing_migration_nineteen_database_gains_exact_policy_schema() {
        let database = database_through_delivery_pause().await;
        assert!(!target_exists(&database).await);
        Migrator::up(&database, Some(1)).await.unwrap();
        validate_schema(&SchemaManager::new(&database))
            .await
            .unwrap();
        assert!(target_exists(&database).await);
        // The table starts empty: no row means no user decision has ever been
        // made, which the policy boundary maps to the closed default.
        assert_eq!(
            database
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {TABLE}"),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "count")
                .unwrap(),
            0
        );
        let pause_type: String = database
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT type FROM sqlite_master WHERE name = '{PAUSE_TABLE}'"),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "type")
            .unwrap();
        assert_eq!(pause_type, "table");
    }

    #[tokio::test]
    async fn valid_policy_rows_round_trip_through_the_check_constraints() {
        let database = migrated_database().await;
        let opaque_source_list =
            "11111111111111111111111111111111-22223333-4444-5555-6666-777788889999";
        let rows = [
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted, consent_locale,
                     consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 0, '', 0, 0, '')"
            ),
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted, consent_locale,
                     consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, 'en-US', 7, 1, '{opaque_source_list}')"
            ),
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted, consent_locale,
                     consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 2, 1, 'en', 2147483647, 0, '')"
            ),
        ];
        for sql in rows {
            database.execute_unprepared(&sql).await.unwrap();
        }
    }

    #[tokio::test]
    async fn check_constraints_reject_every_invariant_violation() {
        let database = migrated_database().await;
        let rejected = [
            // A second row cannot exist.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (2, 1, 0, '', 0, 0, '')"
            ),
            // Generation zero would alias the unpersisted closed default.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 0, 0, '', 0, 0, '')"
            ),
            // Enablement without consent.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 0, '', 0, 1, '')"
            ),
            // Consent with an empty locale.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, '', 1, 0, '')"
            ),
            // Consent with a zero disclosure revision.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, 'en', 0, 0, '')"
            ),
            // A non-ASCII locale tag.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, 'é', 1, 0, '')"
            ),
            // A locale tag above the BCP-47 byte bound.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, '{}', 1, 0, '')",
                "a".repeat(36)
            ),
            // A source list above the bounded maximum.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, 'en', 1, 1, '{}')",
                "a".repeat(MAX_ENABLED_SOURCES_BYTES as usize + 1)
            ),
            // A non-text source list payload.
            format!(
                "INSERT OR REPLACE INTO {TABLE} (slot, policy_generation, consent_granted,
                     consent_locale, consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, 'en', 1, 1, x'00')"
            ),
        ];
        for sql in rejected {
            database.execute_unprepared(&sql).await.unwrap_err();
        }
        assert_eq!(
            database
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {TABLE}"),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "count")
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn exact_retry_is_idempotent_and_near_matches_fail_closed() {
        let exact = database_through_delivery_pause().await;
        Migration.up(&SchemaManager::new(&exact)).await.unwrap();
        Migration.up(&SchemaManager::new(&exact)).await.unwrap();

        let partial = database_through_delivery_pause().await;
        partial
            .execute_unprepared(&format!("CREATE TABLE {TABLE} (slot INTEGER PRIMARY KEY)"))
            .await
            .unwrap();
        let error = Migrator::up(&partial, Some(1)).await.unwrap_err();
        assert!(error.to_string().contains("column schema"));
        assert!(target_exists(&partial).await);

        let weakened = database_through_delivery_pause().await;
        weakened
            .execute_unprepared(&canonical_table_sql().replace(
                "policy_generation BETWEEN 1 AND 9223372036854775807",
                "policy_generation BETWEEN 0 AND 9223372036854775807",
            ))
            .await
            .unwrap();
        let error = Migrator::up(&weakened, Some(1)).await.unwrap_err();
        assert!(error.to_string().contains("exact canonical"));
        assert!(target_exists(&weakened).await);
    }

    #[tokio::test]
    async fn downgrade_is_lossless_only_when_no_decision_was_recorded() {
        let empty = migrated_database().await;
        Migration.down(&SchemaManager::new(&empty)).await.unwrap();
        assert!(!target_exists(&empty).await);
        assert!(empty
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT type FROM sqlite_master WHERE name = '{PAUSE_TABLE}'"),
            ))
            .await
            .unwrap()
            .is_some());

        let decided = migrated_database().await;
        decided
            .execute_unprepared(&format!(
                "INSERT INTO {TABLE} (slot, policy_generation, consent_granted, consent_locale,
                     consent_disclosure_revision, enabled, enabled_sources)
                 VALUES (1, 1, 1, 'en', 1, 0, '')"
            ))
            .await
            .unwrap();
        let error = Migration
            .down(&SchemaManager::new(&decided))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot downgrade"));
        assert!(target_exists(&decided).await);
    }

    #[tokio::test]
    async fn unexpected_schema_objects_are_rejected_without_destruction() {
        let database = migrated_database().await;
        database
            .execute_unprepared(&format!(
                "CREATE TRIGGER unexpected_lastfm_policy_trigger
                 AFTER INSERT ON {TABLE} BEGIN SELECT 1; END"
            ))
            .await
            .unwrap();
        // The ledger is already current, so re-run the migration itself to
        // prove revalidation refuses the unexpected trigger.
        let error = Migration
            .up(&SchemaManager::new(&database))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unexpected trigger"));
        assert!(target_exists(&database).await);
    }

    #[tokio::test]
    async fn missing_prerequisite_pause_table_is_refused() {
        let database = database_through_delivery_pause().await;
        database
            .execute_unprepared(&format!("DROP TABLE {PAUSE_TABLE}"))
            .await
            .unwrap();
        let error = Migrator::up(&database, Some(1)).await.unwrap_err();
        assert!(error.to_string().contains("must exist before"));
        assert!(!target_exists(&database).await);
    }
}
