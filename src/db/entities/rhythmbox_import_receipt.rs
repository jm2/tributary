//! SeaORM entity and validated storage boundary for completed Rhythmbox imports.
//!
//! Receipts intentionally persist only content-redacted digests and a format
//! version. The policy digest must cover every user-selected import policy and
//! the complete root-remap semantics (including whether remapping is absent),
//! so the composite key identifies one exact, repeatable import attempt.

use std::fmt;

use sea_orm::entity::prelude::*;

/// Byte length of the SHA-256 snapshot and selected-policy digests.
pub const RHYTHMBOX_IMPORT_DIGEST_BYTES: usize = 32;
/// First supported canonical importer format.
pub const RHYTHMBOX_IMPORTER_VERSION_V1: i32 = 1;
/// Largest importer version accepted by the persistent format.
pub const MAX_RHYTHMBOX_IMPORTER_VERSION: i32 = i32::MAX;

/// Domain tag prepended to the canonical `rhythmdb.xml`/`playlists.xml`
/// snapshot preimage before hashing.
pub const RHYTHMBOX_SNAPSHOT_DIGEST_DOMAIN: &[u8] = b"tributary:rhythmbox-import:snapshot:v1\0";
/// Domain tag prepended to the canonical selected-policy and root-remap
/// preimage before hashing.
pub const RHYTHMBOX_POLICY_DIGEST_DOMAIN: &[u8] = b"tributary:rhythmbox-import:policy:v1\0";

/// Raw `rhythmbox_import_receipts` row.
///
/// A manual [`Debug`] implementation prevents digest material from being
/// emitted in logs. Although the digests are one-way, they are derived from a
/// user's library snapshot and path-remap policy and remain private evidence.
#[derive(Clone, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "rhythmbox_import_receipts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub snapshot_digest: Vec<u8>,
    #[sea_orm(primary_key, auto_increment = false)]
    pub importer_version: i32,
    #[sea_orm(primary_key, auto_increment = false)]
    pub policy_digest: Vec<u8>,
}

impl fmt::Debug for Model {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxImportReceiptModel")
            .field("snapshot_digest_byte_len", &self.snapshot_digest.len())
            .field("importer_version", &self.importer_version)
            .field("policy_digest_byte_len", &self.policy_digest.len())
            .finish()
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Validated, fixed-width identity of one completed Rhythmbox import.
#[derive(Clone, Eq, PartialEq)]
pub struct StoredRhythmboxImportReceipt {
    pub snapshot_digest: [u8; RHYTHMBOX_IMPORT_DIGEST_BYTES],
    pub importer_version: i32,
    pub policy_digest: [u8; RHYTHMBOX_IMPORT_DIGEST_BYTES],
}

impl fmt::Debug for StoredRhythmboxImportReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredRhythmboxImportReceipt")
            .field("snapshot_digest_byte_len", &self.snapshot_digest.len())
            .field("importer_version", &self.importer_version)
            .field("policy_digest_byte_len", &self.policy_digest.len())
            .finish()
    }
}

impl TryFrom<Model> for StoredRhythmboxImportReceipt {
    type Error = RhythmboxImportReceiptDataError;

    fn try_from(model: Model) -> Result<Self, Self::Error> {
        let snapshot_digest = model
            .snapshot_digest
            .try_into()
            .map_err(|_| RhythmboxImportReceiptDataError::SnapshotDigest)?;
        if !(RHYTHMBOX_IMPORTER_VERSION_V1..=MAX_RHYTHMBOX_IMPORTER_VERSION)
            .contains(&model.importer_version)
        {
            return Err(RhythmboxImportReceiptDataError::ImporterVersion);
        }
        let policy_digest = model
            .policy_digest
            .try_into()
            .map_err(|_| RhythmboxImportReceiptDataError::PolicyDigest)?;

        Ok(Self {
            snapshot_digest,
            importer_version: model.importer_version,
            policy_digest,
        })
    }
}

impl From<StoredRhythmboxImportReceipt> for Model {
    fn from(receipt: StoredRhythmboxImportReceipt) -> Self {
        Self {
            snapshot_digest: receipt.snapshot_digest.to_vec(),
            importer_version: receipt.importer_version,
            policy_digest: receipt.policy_digest.to_vec(),
        }
    }
}

/// Reason an untrusted raw receipt row was not canonical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxImportReceiptDataError {
    SnapshotDigest,
    ImporterVersion,
    PolicyDigest,
}

impl fmt::Display for RhythmboxImportReceiptDataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SnapshotDigest => "Rhythmbox snapshot digest is not canonical",
            Self::ImporterVersion => "Rhythmbox importer version is not canonical",
            Self::PolicyDigest => "Rhythmbox policy digest is not canonical",
        })
    }
}

impl std::error::Error for RhythmboxImportReceiptDataError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            snapshot_digest: vec![0x73; RHYTHMBOX_IMPORT_DIGEST_BYTES],
            importer_version: RHYTHMBOX_IMPORTER_VERSION_V1,
            policy_digest: vec![0x70; RHYTHMBOX_IMPORT_DIGEST_BYTES],
        }
    }

    #[test]
    fn conversion_requires_exact_fixed_width_digests_and_positive_version() {
        let expected = StoredRhythmboxImportReceipt::try_from(model()).expect("valid receipt");
        assert_eq!(expected.snapshot_digest, [0x73; 32]);
        assert_eq!(expected.policy_digest, [0x70; 32]);
        assert_eq!(expected.importer_version, 1);
        assert_eq!(Model::from(expected), model());

        let mut invalid = model();
        invalid.snapshot_digest.pop();
        assert_eq!(
            StoredRhythmboxImportReceipt::try_from(invalid).unwrap_err(),
            RhythmboxImportReceiptDataError::SnapshotDigest
        );

        let mut invalid = model();
        invalid.importer_version = 0;
        assert_eq!(
            StoredRhythmboxImportReceipt::try_from(invalid).unwrap_err(),
            RhythmboxImportReceiptDataError::ImporterVersion
        );

        let mut invalid = model();
        invalid.policy_digest.push(0);
        assert_eq!(
            StoredRhythmboxImportReceipt::try_from(invalid).unwrap_err(),
            RhythmboxImportReceiptDataError::PolicyDigest
        );
    }

    #[test]
    fn debug_output_discloses_only_digest_lengths() {
        let raw = model();
        let stored = StoredRhythmboxImportReceipt::try_from(raw.clone()).unwrap();
        for diagnostics in [format!("{raw:?}"), format!("{stored:?}")] {
            assert!(diagnostics.contains("digest_byte_len: 32"));
            assert!(!diagnostics.contains("115"));
            assert!(!diagnostics.contains("112"));
            assert!(!diagnostics.contains("737373"));
            assert!(!diagnostics.contains("707070"));
        }
    }

    #[test]
    fn digest_domains_are_distinct_versioned_and_nul_terminated() {
        assert_ne!(
            RHYTHMBOX_SNAPSHOT_DIGEST_DOMAIN,
            RHYTHMBOX_POLICY_DIGEST_DOMAIN
        );
        for domain in [
            RHYTHMBOX_SNAPSHOT_DIGEST_DOMAIN,
            RHYTHMBOX_POLICY_DIGEST_DOMAIN,
        ] {
            assert!(domain.ends_with(&[0]));
            assert!(domain.windows(3).any(|window| window == b":v1"));
        }
    }
}
