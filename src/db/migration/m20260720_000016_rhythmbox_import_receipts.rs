//! Migration: persist content-redacted Rhythmbox import receipts.
//!
//! A receipt identifies only an exact canonical input snapshot, importer
//! version, and selected-policy/root-remap digest. It deliberately stores no
//! source paths, root names, track metadata, playlist names, XML, or created
//! object identities. Import mutations and the receipt insertion are expected
//! to share one caller-owned transaction, making an exact retry a no-op.

use std::fmt;

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{
    ConnectionTrait, DatabaseConnection, Statement, TransactionTrait,
};

const TABLE: &str = "rhythmbox_import_receipts";
const DIGEST_BYTES: usize = 32;
const MIN_IMPORTER_VERSION: i32 = 1;
const MAX_IMPORTER_VERSION: i32 = i32::MAX;

const SNAPSHOT_DIGEST_CHECK: &str = "ck_rhythmbox_import_receipts_snapshot_digest";
const IMPORTER_VERSION_CHECK: &str = "ck_rhythmbox_import_receipts_importer_version";
const POLICY_DIGEST_CHECK: &str = "ck_rhythmbox_import_receipts_policy_digest";
const RECEIPT_PRIMARY_KEY: &str = "pk_rhythmbox_import_receipts";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_receipts(manager, true).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_receipts(manager, false).await
    }
}

/// Revalidate the receipt boundary even when the migration ledger is already
/// current. A modified table could otherwise silently weaken exact retry
/// identity or admit malformed digest rows.
pub(super) async fn revalidate(connection: &DatabaseConnection) -> Result<(), DbErr> {
    validate_schema(&SchemaManager::new(connection)).await
}

/// Own the complete DDL operation so a validation or lossless-downgrade
/// refusal cannot partially alter the target.
async fn migrate_receipts(manager: &SchemaManager<'_>, create: bool) -> Result<(), DbErr> {
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
            Err(preserve_original_error(error, rollback))
        }
    }
}

fn preserve_original_error(original: DbErr, rollback: Result<(), DbErr>) -> DbErr {
    match rollback {
        Ok(()) => original,
        Err(rollback_error) => DbErr::Migration(format!(
            "{original}; additionally failed to roll back Rhythmbox import-receipt migration: \
             {rollback_error}"
        )),
    }
}

async fn create_or_validate(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    match object_type(manager, TABLE).await? {
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
    let Some(target_type) = object_type(manager, TABLE).await? else {
        return Ok(());
    };
    if target_type != "table" {
        return Err(DbErr::Migration(format!(
            "{TABLE} must be a table, found {target_type}"
        )));
    }
    validate_schema(manager).await?;

    let row = manager
        .get_connection()
        .query_one(Statement::from_string(
            manager.get_database_backend(),
            format!("SELECT COUNT(*) AS count FROM {TABLE}"),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("failed to inspect Rhythmbox receipts".to_string()))?;
    let count: i64 = row.try_get("", "count")?;
    if count != 0 {
        return Err(DbErr::Migration(format!(
            "cannot downgrade {count} Rhythmbox import receipt row(s) losslessly"
        )));
    }

    manager
        .get_connection()
        .execute_unprepared(&format!("DROP TABLE {TABLE}"))
        .await?;
    if object_type(manager, TABLE).await?.is_some() {
        return Err(DbErr::Migration(format!(
            "{TABLE} remained after downgrade"
        )));
    }
    Ok(())
}

fn canonical_table_sql() -> String {
    format!(
        "CREATE TABLE {TABLE} (
             snapshot_digest BLOB NOT NULL,
             importer_version INTEGER NOT NULL,
             policy_digest BLOB NOT NULL,
             CONSTRAINT {SNAPSHOT_DIGEST_CHECK} CHECK (
                 typeof(snapshot_digest) = 'blob'
                 AND length(snapshot_digest) = {DIGEST_BYTES}
             ),
             CONSTRAINT {IMPORTER_VERSION_CHECK} CHECK (
                 typeof(importer_version) = 'integer'
                 AND importer_version BETWEEN {MIN_IMPORTER_VERSION} AND {MAX_IMPORTER_VERSION}
             ),
             CONSTRAINT {POLICY_DIGEST_CHECK} CHECK (
                 typeof(policy_digest) = 'blob'
                 AND length(policy_digest) = {DIGEST_BYTES}
             ),
             CONSTRAINT {RECEIPT_PRIMARY_KEY}
                 PRIMARY KEY (snapshot_digest, importer_version, policy_digest)
         ) WITHOUT ROWID"
    )
}

async fn object_type(manager: &SchemaManager<'_>, name: &str) -> Result<Option<String>, DbErr> {
    manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
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
    validate_columns(manager).await?;
    validate_table_sql(manager).await?;
    validate_indexes(manager).await?;
    validate_no_foreign_keys(manager).await?;
    validate_no_triggers(manager).await
}

async fn validate_columns(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA table_info('{TABLE}')"),
        ))
        .await?
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
        (
            0,
            "snapshot_digest".to_string(),
            "blob".to_string(),
            1,
            None,
            1,
        ),
        (
            1,
            "importer_version".to_string(),
            "integer".to_string(),
            1,
            None,
            2,
        ),
        (
            2,
            "policy_digest".to_string(),
            "blob".to_string(),
            1,
            None,
            3,
        ),
    ];
    if columns != expected {
        return Err(DbErr::Migration(format!(
            "{TABLE} has an unexpected column schema: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_table_sql(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
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
    Ok(())
}

async fn validate_indexes(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_list('{TABLE}')"),
        ))
        .await?;
    let indexes = rows
        .into_iter()
        .map(|row| {
            Ok::<_, DbErr>((
                row.try_get::<String>("", "name")?,
                row.try_get::<i32>("", "unique")? == 1,
                row.try_get::<String>("", "origin")?,
                row.try_get::<i32>("", "partial")? == 1,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [(primary_name, true, origin, false)] = indexes.as_slice() else {
        return Err(DbErr::Migration(format!(
            "{TABLE} has unexpected indexes: {indexes:?}"
        )));
    };
    if origin != "pk" {
        return Err(DbErr::Migration(format!(
            "{TABLE} composite-key index has origin {origin}"
        )));
    }

    let columns = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA index_info('{}')", primary_name.replace('\'', "''")),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    let expected = ["snapshot_digest", "importer_version", "policy_digest"];
    if columns.iter().map(String::as_str).ne(expected) {
        return Err(DbErr::Migration(format!(
            "{TABLE} composite key has unexpected columns: {columns:?}"
        )));
    }
    Ok(())
}

async fn validate_no_foreign_keys(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let rows = manager
        .get_connection()
        .query_all(Statement::from_string(
            manager.get_database_backend(),
            format!("PRAGMA foreign_key_list('{TABLE}')"),
        ))
        .await?;
    if !rows.is_empty() {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {} unexpected foreign key(s)",
            rows.len()
        )));
    }
    Ok(())
}

async fn validate_no_triggers(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let row = manager
        .get_connection()
        .query_one(Statement::from_sql_and_values(
            manager.get_database_backend(),
            "SELECT COUNT(*) AS count FROM sqlite_master
             WHERE type = 'trigger' AND tbl_name = ?",
            [TABLE.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("failed to inspect {TABLE} triggers")))?;
    let count: i64 = row.try_get("", "count")?;
    if count != 0 {
        return Err(DbErr::Migration(format!(
            "{TABLE} has {count} unexpected trigger(s)"
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
        formatter.write_str("RhythmboxImportReceiptsMigration")
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait,
        PaginatorTrait, Statement, TransactionTrait,
    };

    use super::*;
    use crate::db::entities::rhythmbox_import_receipt::{self, StoredRhythmboxImportReceipt};
    use crate::db::migration::Migrator;

    async fn database_before_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory SQLite database");
        Migrator::up(&db, Some(15))
            .await
            .expect("apply migrations preceding Rhythmbox receipts");
        db
    }

    async fn migrated_database() -> DatabaseConnection {
        let db = database_before_migration().await;
        Migrator::up(&db, Some(1))
            .await
            .expect("apply Rhythmbox receipt migration");
        db
    }

    fn receipt_model(
        snapshot_byte: u8,
        importer_version: i32,
        policy_byte: u8,
    ) -> rhythmbox_import_receipt::Model {
        rhythmbox_import_receipt::Model {
            snapshot_digest: vec![snapshot_byte; DIGEST_BYTES],
            importer_version,
            policy_digest: vec![policy_byte; DIGEST_BYTES],
        }
    }

    async fn insert_receipt(
        db: &impl ConnectionTrait,
        model: rhythmbox_import_receipt::Model,
    ) -> Result<rhythmbox_import_receipt::Model, DbErr> {
        let active: rhythmbox_import_receipt::ActiveModel = model.into();
        active.insert(db).await
    }

    async fn migration_is_applied(db: &DatabaseConnection) -> bool {
        let name = Migration.name().to_string();
        Migrator::get_migration_models(db)
            .await
            .expect("read migration ledger")
            .iter()
            .any(|migration| migration.version == name)
    }

    async fn target_exists(db: &DatabaseConnection) -> bool {
        object_type(&SchemaManager::new(db), TABLE)
            .await
            .expect("inspect receipt table")
            == Some("table".to_string())
    }

    #[tokio::test]
    async fn fresh_up_creates_exact_schema_and_entity_round_trips() {
        let db = migrated_database().await;
        validate_schema(&SchemaManager::new(&db))
            .await
            .expect("canonical receipt schema");
        assert!(migration_is_applied(&db).await);
        assert_eq!(
            DIGEST_BYTES,
            crate::db::entities::rhythmbox_import_receipt::RHYTHMBOX_IMPORT_DIGEST_BYTES
        );
        assert_eq!(
            MIN_IMPORTER_VERSION,
            crate::db::entities::rhythmbox_import_receipt::RHYTHMBOX_IMPORTER_VERSION_V1
        );
        assert_eq!(
            MAX_IMPORTER_VERSION,
            crate::db::entities::rhythmbox_import_receipt::MAX_RHYTHMBOX_IMPORTER_VERSION
        );

        let expected = receipt_model(0xa5, 7, 0x5a);
        insert_receipt(&db, expected.clone())
            .await
            .expect("insert receipt through entity");
        let raw = rhythmbox_import_receipt::Entity::find_by_id((
            expected.snapshot_digest.clone(),
            expected.importer_version,
            expected.policy_digest.clone(),
        ))
        .one(&db)
        .await
        .expect("query receipt")
        .expect("receipt exists");
        assert_eq!(raw, expected);
        let stored = StoredRhythmboxImportReceipt::try_from(raw).expect("validate receipt");
        assert_eq!(stored.snapshot_digest, [0xa5; DIGEST_BYTES]);
        assert_eq!(stored.policy_digest, [0x5a; DIGEST_BYTES]);
        assert_eq!(stored.importer_version, 7);
    }

    #[tokio::test]
    async fn exact_interrupted_target_is_retryable_but_partial_or_near_match_is_refused() {
        let compatible = database_before_migration().await;
        Migration
            .up(&SchemaManager::new(&compatible))
            .await
            .expect("install exact interrupted target");
        assert!(!migration_is_applied(&compatible).await);
        Migrator::up(&compatible, Some(1))
            .await
            .expect("record exact interrupted target");
        assert!(migration_is_applied(&compatible).await);
        Migration
            .up(&SchemaManager::new(&compatible))
            .await
            .expect("exact retry is idempotent");

        let partial = database_before_migration().await;
        partial
            .execute_unprepared(
                "CREATE TABLE rhythmbox_import_receipts (
                     snapshot_digest BLOB PRIMARY KEY NOT NULL
                 )",
            )
            .await
            .expect("install partial target");
        let error = Migrator::up(&partial, Some(1))
            .await
            .expect_err("partial target must fail closed");
        assert!(error.to_string().contains("column schema"));
        assert!(!migration_is_applied(&partial).await);
        assert!(target_exists(&partial).await);

        let near_match = database_before_migration().await;
        let altered = canonical_table_sql().replace(
            "importer_version BETWEEN 1 AND 2147483647",
            "importer_version BETWEEN 2 AND 2147483647",
        );
        near_match
            .execute_unprepared(&altered)
            .await
            .expect("install restrictive lookalike");
        let error = Migrator::up(&near_match, Some(1))
            .await
            .expect_err("near-match CHECK must fail closed");
        assert!(error.to_string().contains("exact canonical"));
        assert!(!migration_is_applied(&near_match).await);
    }

    #[tokio::test]
    async fn object_collision_is_refused_without_replacing_existing_data() {
        let db = database_before_migration().await;
        db.execute_unprepared(
            "CREATE VIEW rhythmbox_import_receipts AS
             SELECT id AS opaque_data FROM playlists",
        )
        .await
        .expect("create conflicting view");
        let error = Migrator::up(&db, Some(1))
            .await
            .expect_err("view collision must fail");
        assert!(error.to_string().contains("must be a table"));
        assert_eq!(
            object_type(&SchemaManager::new(&db), TABLE).await.unwrap(),
            Some("view".to_string())
        );
        assert!(!migration_is_applied(&db).await);
    }

    #[tokio::test]
    async fn database_constraints_reject_noncanonical_storage_and_boundaries_hold() {
        let db = migrated_database().await;
        for valid_version in [MIN_IMPORTER_VERSION, MAX_IMPORTER_VERSION] {
            insert_receipt(&db, receipt_model(valid_version as u8, valid_version, 0x41))
                .await
                .expect("importer-version boundary is valid");
        }

        let mut invalid = Vec::new();
        let mut row = receipt_model(1, 1, 2);
        row.snapshot_digest.pop();
        invalid.push(row);
        let mut row = receipt_model(1, 1, 2);
        row.snapshot_digest.push(0);
        invalid.push(row);
        let mut row = receipt_model(1, 1, 2);
        row.importer_version = 0;
        invalid.push(row);
        let mut row = receipt_model(1, 1, 2);
        row.policy_digest.pop();
        invalid.push(row);
        let mut row = receipt_model(1, 1, 2);
        row.policy_digest.push(0);
        invalid.push(row);
        for row in invalid {
            insert_receipt(&db, row)
                .await
                .expect_err("noncanonical entity row must violate a CHECK");
        }

        for sql in [
            "INSERT INTO rhythmbox_import_receipts
             VALUES ('01234567890123456789012345678901', 4, zeroblob(32))",
            "INSERT INTO rhythmbox_import_receipts
             VALUES (zeroblob(32), 4.5, zeroblob(32))",
            "INSERT INTO rhythmbox_import_receipts
             VALUES (zeroblob(32), 4, '01234567890123456789012345678901')",
            "INSERT INTO rhythmbox_import_receipts
             VALUES (zeroblob(32), 2147483648, zeroblob(32))",
        ] {
            db.execute_unprepared(sql)
                .await
                .expect_err("noncanonical SQLite storage must fail closed");
        }
        assert_eq!(
            rhythmbox_import_receipt::Entity::find()
                .count(&db)
                .await
                .expect("count canonical receipts"),
            2
        );
    }

    #[tokio::test]
    async fn composite_key_distinguishes_snapshot_version_and_complete_policy() {
        let db = migrated_database().await;
        let exact = receipt_model(0x11, 1, 0x21);
        insert_receipt(&db, exact.clone())
            .await
            .expect("first exact receipt");
        insert_receipt(&db, exact.clone())
            .await
            .expect_err("duplicate exact receipt is unique");

        for distinct in [
            receipt_model(0x12, 1, 0x21),
            receipt_model(0x11, 2, 0x21),
            receipt_model(0x11, 1, 0x22),
        ] {
            insert_receipt(&db, distinct)
                .await
                .expect("one changed identity dimension is a new attempt");
        }

        let result = db
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO rhythmbox_import_receipts (
                     snapshot_digest, importer_version, policy_digest
                 ) VALUES (?, ?, ?)
                 ON CONFLICT(snapshot_digest, importer_version, policy_digest) DO NOTHING",
                [
                    exact.snapshot_digest.into(),
                    exact.importer_version.into(),
                    exact.policy_digest.into(),
                ],
            ))
            .await
            .expect("atomic exact-retry probe");
        assert_eq!(result.rows_affected(), 0);
        assert_eq!(
            rhythmbox_import_receipt::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            4
        );
    }

    #[tokio::test]
    async fn caller_transaction_can_commit_or_rollback_mutations_with_the_receipt() {
        let db = migrated_database().await;
        db.execute_unprepared(
            "INSERT INTO tracks (
                 id, file_path, title, artist_name, album_title, play_count,
                 date_added, date_modified
             ) VALUES (
                 'local-track', '/music/local.flac', 'Title', 'Artist', 'Album', 1,
                 '2026-07-20', '2026-07-20'
             )",
        )
        .await
        .expect("insert local track");

        let transaction = db.begin().await.expect("begin import transaction");
        transaction
            .execute_unprepared("UPDATE tracks SET play_count = 99 WHERE id = 'local-track'")
            .await
            .expect("stage imported play count");
        insert_receipt(&transaction, receipt_model(0x31, 1, 0x41))
            .await
            .expect("stage receipt");
        transaction
            .rollback()
            .await
            .expect("roll back failed import");

        let track = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT play_count FROM tracks WHERE id = 'local-track'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(track.try_get::<i32>("", "play_count").unwrap(), 1);
        assert_eq!(
            rhythmbox_import_receipt::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn unexpected_schema_objects_are_never_accepted_or_destroyed_by_down() {
        let db = migrated_database().await;
        db.execute_unprepared(
            "CREATE INDEX idx_unexpected_rhythmbox_receipt
             ON rhythmbox_import_receipts (importer_version)",
        )
        .await
        .expect("install unexpected index");
        let error = Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect_err("retry must reject unexpected index");
        assert!(error.to_string().contains("unexpected indexes"));
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("down must preserve unknown index");
        assert!(target_exists(&db).await);
        assert_eq!(
            object_type(&SchemaManager::new(&db), "idx_unexpected_rhythmbox_receipt")
                .await
                .unwrap(),
            Some("index".to_string())
        );

        db.execute_unprepared("DROP INDEX idx_unexpected_rhythmbox_receipt")
            .await
            .unwrap();
        db.execute_unprepared(
            "CREATE TRIGGER unexpected_rhythmbox_receipt_trigger
             AFTER INSERT ON rhythmbox_import_receipts BEGIN SELECT 1; END",
        )
        .await
        .expect("install unexpected trigger");
        let error = Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("down must preserve unknown trigger");
        assert!(error.to_string().contains("unexpected trigger"));
        assert!(target_exists(&db).await);
    }

    #[tokio::test]
    async fn empty_downgrade_is_exact_and_repeatable() {
        let db = migrated_database().await;
        Migrator::down(&db, Some(1))
            .await
            .expect("empty receipt table is losslessly removable");
        assert!(!target_exists(&db).await);
        assert!(!migration_is_applied(&db).await);
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("repeated direct down is idempotent");
    }

    #[tokio::test]
    async fn nonempty_downgrade_refuses_without_disclosing_or_losing_receipt() {
        let db = migrated_database().await;
        let private = receipt_model(0x52, 1, 0x50);
        insert_receipt(&db, private.clone())
            .await
            .expect("insert private receipt");

        let error = Migrator::down(&db, Some(1))
            .await
            .expect_err("nonempty receipt table cannot downgrade losslessly");
        let diagnostics = error.to_string();
        assert!(diagnostics.contains("1 Rhythmbox import receipt row"));
        assert!(!diagnostics.contains("82"));
        assert!(!diagnostics.contains("80"));
        assert!(migration_is_applied(&db).await);
        validate_schema(&SchemaManager::new(&db))
            .await
            .expect("failed downgrade retains canonical schema");
        assert_eq!(
            rhythmbox_import_receipt::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            1
        );

        rhythmbox_import_receipt::Entity::delete_by_id((
            private.snapshot_digest,
            private.importer_version,
            private.policy_digest,
        ))
        .exec(&db)
        .await
        .expect("explicitly remove receipt before downgrade");
        Migrator::down(&db, Some(1))
            .await
            .expect("downgrade after explicit removal");
        assert!(!target_exists(&db).await);
    }

    #[tokio::test]
    async fn malformed_target_is_never_dropped_by_down() {
        let db = database_before_migration().await;
        db.execute_unprepared(
            "CREATE TABLE rhythmbox_import_receipts (
                 opaque_key BLOB PRIMARY KEY NOT NULL,
                 opaque_data VARCHAR NOT NULL
             )",
        )
        .await
        .expect("install unknown target");
        db.execute_unprepared(
            "INSERT INTO rhythmbox_import_receipts (opaque_key, opaque_data)
             VALUES (X'01', 'do-not-drop')",
        )
        .await
        .expect("insert unknown data");
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect_err("down must not guess at a partial target");
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT opaque_data FROM rhythmbox_import_receipts".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<String>("", "opaque_data").unwrap(),
            "do-not-drop"
        );
    }
}
