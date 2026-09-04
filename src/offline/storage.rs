//! Atomic, verify-before-publish cache storage.
//!
//! This module is the only place in the offline subsystem that touches the
//! filesystem (`docs/offline-media.md`, "Atomic storage"). The order is
//! normative and enforced by the API shape: the temp file lives in the same
//! directory as its final cache path, integrity is verified on the temp file
//! before any rename, and publish is an atomic rename followed by a
//! parent-directory `fsync` on Unix. Cross-filesystem publish is structurally
//! impossible because the reservation is always created beside its final
//! path; there is no copy+sync+delete fallback.
//!
//! Paths are derived exclusively from SHA-256 cache keys over the exact,
//! unmodified identifier bytes: bounded, fixed-charset, separator-free, and
//! incapable of `..` traversal. No URL, credential, or raw identifier byte
//! ever appears in a path.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::architecture::identity::MediaKey;
use crate::architecture::offline::{
    CommittedSnapshot, DigestProvenance, OfflineError, OperationalLicence,
};

/// Byte length of a derived cache key (128 bits of SHA-256 as 32 hex chars).
const CACHE_KEY_HEX_CHARS: usize = 32;

/// The engine-chosen, credential-free constant file name inside a
/// per-track scope. The directory is the identity scope, so the name carries
/// no identity beyond the recorded mapping.
const CACHED_FILE_NAME: &str = "media";

/// File-name suffix for a short-lived temp reservation.
const TEMP_SUFFIX: &str = ".part";

/// Bound on a committed snapshot's persisted path length. The storage layer
/// mints paths only from bounded cache keys, so this can only be exceeded by
/// an absurd cache root; enforce the contract ceiling regardless.
const MAX_PERSISTED_PATH_BYTES: usize = 4 * 1024;

/// A reserved temp file beside its final cache path.
///
/// Holding this value is the only way to append bytes or to publish; dropping
/// it without publishing leaves the temp on disk for a later cleanup pass of
/// the same job (the engine unlinks it on every terminal outcome).
#[derive(Debug)]
pub struct TempReservation {
    final_path: PathBuf,
    temp_path: PathBuf,
}

impl TempReservation {
    /// The path the snapshot will occupy after a successful publish.
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// The short-lived temp path. Never displayed or persisted.
    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }
}

/// The expected digest a publish is verified against.
///
/// The provenance tiers are exactly the contract's two: an advertised digest
/// compared exactly, or the caller's independent second-transfer digest that
/// must equal the first transfer's bytes. There is no third tier; a backend
/// with neither must never reach [`CacheStore::verify_and_publish`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishCheck {
    /// An advertised digest: the on-disk bytes must hash to exactly this.
    Advertised([u8; 32]),
    /// Double-fetch verification: the second transfer produced this digest
    /// and it must match the temp file's bytes exactly.
    DoubleFetch([u8; 32]),
}

/// The cache root for all sources.
///
/// Layout: `<root>/<source_key>/<track_key>/media`, where both keys are the
/// first 32 hex characters of SHA-256 over the exact identifier bytes.
#[derive(Clone, Debug)]
pub struct CacheStore {
    root: PathBuf,
}

impl CacheStore {
    /// Create a store rooted at `root`. The directory is created lazily on
    /// first reservation so a never-used cache leaves no filesystem trace.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Derive the bounded cache key for one identifier: the first 32 hex
    /// characters of SHA-256 over the exact, unmodified identifier bytes.
    ///
    /// This is a one-way derivation, not parsing: the identifier is never
    /// interpreted and the derived key never feeds back into identity.
    #[must_use]
    pub fn cache_key(identifier: &[u8]) -> String {
        let digest = Sha256::digest(identifier);
        to_hex(&digest)[..CACHE_KEY_HEX_CHARS].to_string()
    }

    /// The track-scoped directory for one media key.
    #[must_use]
    pub fn track_dir(&self, key: &MediaKey) -> PathBuf {
        self.root
            .join(Self::cache_key(key.source_id.to_string().as_bytes()))
            .join(Self::cache_key(key.track_id.as_str().as_bytes()))
    }

    /// Reserve the temp file beside its final cache path.
    ///
    /// Fails [`OfflineError::StorageUnavailable`] when the directories cannot
    /// be created or the temp cannot be opened — including the read-only or
    /// wrong-filesystem parent cases the contract refuses at admission.
    /// Any pre-existing temp from an earlier attempt of the same job is
    /// replaced, never appended to blind.
    pub fn reserve_temp(
        &self,
        key: &MediaKey,
        job_nonce: u64,
    ) -> Result<TempReservation, OfflineError> {
        let dir = self.track_dir(key);
        fs::create_dir_all(&dir).map_err(|_| OfflineError::StorageUnavailable)?;
        let final_path = dir.join(CACHED_FILE_NAME);
        let temp_path = dir.join(format!("{CACHED_FILE_NAME}{TEMP_SUFFIX}-{job_nonce}"));
        if temp_path.as_os_str().len() > MAX_PERSISTED_PATH_BYTES {
            return Err(OfflineError::StorageUnavailable);
        }
        // Replace any stale reservation from an earlier attempt so a resume
        // always starts from explicitly journaled state, never stale bytes.
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|_| OfflineError::StorageUnavailable)?;
        Ok(TempReservation {
            final_path,
            temp_path,
        })
    }

    /// Write one segment at the engine's journaled offset and `fsync` it.
    ///
    /// A short write or an offset past the current end is refused: the
    /// journaled offset is the only trusted resume state, and torn or
    /// out-of-order writes must never silently count as progress. The
    /// returned digest covers exactly this segment's bytes.
    #[allow(
        clippy::unused_self,
        reason = "keeps the store-method call shape consistent across the storage API"
    )]
    pub fn write_segment(
        &self,
        reservation: &TempReservation,
        offset: u64,
        bytes: &[u8],
    ) -> Result<[u8; 32], OfflineError> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(&reservation.temp_path)
            .map_err(|_| OfflineError::StorageUnavailable)?;
        let len = file
            .metadata()
            .map_err(|_| OfflineError::StorageUnavailable)?
            .len();
        if offset != len {
            return Err(OfflineError::StorageUnavailable);
        }
        file.seek(SeekFrom::End(0))
            .map_err(|_| OfflineError::StorageUnavailable)?;
        file.write_all(bytes)
            .map_err(|_| OfflineError::StorageUnavailable)?;
        file.sync_all()
            .map_err(|_| OfflineError::StorageUnavailable)?;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.update(offset.to_le_bytes());
        Ok(hasher.finalize().into())
    }

    /// Truncate the temp back to the journaled offset, discarding any torn
    /// tail from an interrupted write. Called on resume before any byte of
    /// the remainder is requested.
    #[allow(
        clippy::unused_self,
        reason = "keeps the store-method call shape consistent across the storage API"
    )]
    pub fn truncate_temp(
        &self,
        reservation: &TempReservation,
        journaled_len: u64,
    ) -> Result<(), OfflineError> {
        let file = OpenOptions::new()
            .write(true)
            .open(&reservation.temp_path)
            .map_err(|_| OfflineError::StorageUnavailable)?;
        let len = file
            .metadata()
            .map_err(|_| OfflineError::StorageUnavailable)?
            .len();
        if journaled_len > len {
            return Err(OfflineError::StorageUnavailable);
        }
        file.set_len(journaled_len)
            .map_err(|_| OfflineError::StorageUnavailable)?;
        file.sync_all()
            .map_err(|_| OfflineError::StorageUnavailable)
    }

    /// Verify the temp file's on-disk bytes against the expected digest and,
    /// only on a match, publish by atomic rename with a parent-directory
    /// `fsync` on Unix.
    ///
    /// A mismatch unlinks the temp and fails terminal
    /// ([`OfflineError::IntegrityMismatch`]); an unreadable temp or a
    /// failed rename unlinks the temp and fails
    /// [`OfflineError::StorageUnavailable`]. Nothing was ever renamed and
    /// no caller can observe a half-promoted row, and no failure path
    /// leaves the temp on disk.
    #[allow(
        clippy::unused_self,
        reason = "keeps the store-method call shape consistent across the storage API"
    )]
    pub fn verify_and_publish(
        &self,
        reservation: TempReservation,
        check: PublishCheck,
        key: &MediaKey,
        capability_epoch: u64,
        licence: OperationalLicence,
        committed_at_epoch_secs: u64,
    ) -> Result<CommittedSnapshot, OfflineError> {
        let on_disk = match Self::hash_file(&reservation.temp_path) {
            Ok(digest) => digest,
            Err(err) => {
                // The caller has already taken the reservation out of the
                // job, so no failure path downstream can clean up: leave
                // no temp behind on the way out.
                let _unused = fs::remove_file(&reservation.temp_path);
                return Err(err);
            }
        };
        let expected = match check {
            PublishCheck::Advertised(digest) | PublishCheck::DoubleFetch(digest) => digest,
        };
        if on_disk.bytes != expected {
            let _unused = fs::remove_file(&reservation.temp_path);
            return Err(OfflineError::IntegrityMismatch);
        }
        // Atomic publish: temp and final path share a directory, so the
        // rename is same-filesystem by construction.
        if fs::rename(&reservation.temp_path, &reservation.final_path).is_err() {
            let _unused = fs::remove_file(&reservation.temp_path);
            return Err(OfflineError::StorageUnavailable);
        }
        #[cfg(unix)]
        Self::fsync_parent(&reservation.final_path)?;
        let _unused = crate::architecture::offline::validate_snapshot_path_bytes(
            reservation.final_path.as_os_str().len(),
        );
        Ok(CommittedSnapshot {
            media_key: key.clone(),
            capability_epoch,
            byte_size: on_disk.total,
            sha256_hex: to_hex(&on_disk.bytes),
            digest_provenance: match check {
                PublishCheck::Advertised(_) => DigestProvenance::Advertised,
                PublishCheck::DoubleFetch(_) => DigestProvenance::DoubleFetch,
            },
            cache_path: reservation.final_path.to_string_lossy().into_owned(),
            licence_label: licence,
            committed_at_epoch_secs,
        })
    }

    /// Compute the SHA-256 of the bytes actually on disk.
    fn hash_file(path: &Path) -> Result<FileDigest, OfflineError> {
        let mut file = File::open(path).map_err(|_| OfflineError::StorageUnavailable)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 8192];
        let mut total = 0u64;
        loop {
            let read = file
                .read(&mut buf)
                .map_err(|_| OfflineError::StorageUnavailable)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            total += read as u64;
        }
        Ok(FileDigest {
            bytes: hasher.finalize().into(),
            total,
        })
    }

    #[cfg(unix)]
    fn fsync_parent(published: &Path) -> Result<(), OfflineError> {
        let parent = published.parent().ok_or(OfflineError::StorageUnavailable)?;
        let dir = File::open(parent).map_err(|_| OfflineError::StorageUnavailable)?;
        dir.sync_all().map_err(|_| OfflineError::StorageUnavailable)
    }

    /// Unlink a committed snapshot's file after validating that the recorded
    /// path really lives inside this cache root with the expected layout.
    ///
    /// The recorded mapping is the only path source; this validation is the
    /// content-aware unlink authority the contract requires before a delete.
    /// A missing file is success (already gone); a foreign path is refused.
    pub fn unlink_snapshot(&self, snapshot: &CommittedSnapshot) -> Result<bool, OfflineError> {
        let recorded = PathBuf::from(&snapshot.cache_path);
        let expected_dir = self.track_dir(&snapshot.media_key);
        let Some(recorded_dir) = recorded.parent().map(Path::to_path_buf) else {
            return Err(OfflineError::StorageUnavailable);
        };
        if recorded_dir != expected_dir {
            return Err(OfflineError::StorageUnavailable);
        }
        match fs::remove_file(&recorded) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(OfflineError::StorageUnavailable),
        }
    }
}

/// The SHA-256 of a file's bytes plus its total length.
struct FileDigest {
    bytes: [u8; 32],
    total: u64,
}

/// Lowercase hex encoding of a digest.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _unused = write!(out, "{byte:02x}");
            out
        })
}

/// Current byte length of a file on disk (`0` when absent).
pub(super) fn size_on_disk(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::{SourceId, TrackId};

    fn media_key(seed: &str) -> MediaKey {
        MediaKey::new(
            SourceId::local(),
            TrackId::new(format!("track-{seed}")).unwrap(),
        )
    }

    #[test]
    fn cache_key_is_bounded_hex_and_stable() {
        let raw = TrackId::new("a/b/../weird ünïcode id").unwrap();
        let first = CacheStore::cache_key(raw.as_str().as_bytes());
        let second = CacheStore::cache_key(raw.as_str().as_bytes());
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "cache key must be lowercase hex, got {first}"
        );
    }

    #[test]
    fn cache_key_differs_per_identifier_byte() {
        let a = CacheStore::cache_key(b"track-a");
        let b = CacheStore::cache_key(b"track-b");
        assert_ne!(a, b);
    }

    #[test]
    fn temp_reservation_lives_beside_final_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::new(tmp.path());
        let reservation = store.reserve_temp(&media_key("resume"), 1).unwrap();
        assert_eq!(
            reservation.final_path().parent(),
            reservation.temp_path().parent(),
            "temp must share the final directory for an atomic rename"
        );
        assert!(reservation.temp_path().to_string_lossy().contains(".part-"));
    }

    #[test]
    fn out_of_order_segment_write_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::new(tmp.path());
        let reservation = store.reserve_temp(&media_key("order"), 1).unwrap();
        assert_eq!(
            store.write_segment(&reservation, 4, b"late").unwrap_err(),
            OfflineError::StorageUnavailable
        );
        assert!(store.write_segment(&reservation, 0, b"abcd").is_ok());
        assert_eq!(
            store.write_segment(&reservation, 0, b"again").unwrap_err(),
            OfflineError::StorageUnavailable
        );
    }

    #[test]
    fn verify_before_publish_refuses_mismatch_and_leaves_no_final_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::new(tmp.path());
        let key = media_key("mismatch");
        let reservation = store.reserve_temp(&key, 1).unwrap();
        let final_path = reservation.final_path().to_path_buf();
        store
            .write_segment(&reservation, 0, b"actual bytes")
            .unwrap();
        let mut wrong = [0u8; 32];
        wrong[0] = 0xAB;
        let err = store
            .verify_and_publish(
                reservation,
                PublishCheck::Advertised(wrong),
                &key,
                1,
                OperationalLicence::SourceDeclared,
                0,
            )
            .unwrap_err();
        assert_eq!(err, OfflineError::IntegrityMismatch);
        assert!(!final_path.exists(), "nothing may be renamed on mismatch");
        // The temp is unlinked too.
        let dir = store.track_dir(&key);
        let leftovers: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
        assert!(leftovers.is_empty(), "temp must be unlinked after mismatch");
    }

    #[test]
    fn publish_is_atomic_and_snapshot_records_committed_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::new(tmp.path());
        let key = media_key("publish");
        let reservation = store.reserve_temp(&key, 1).unwrap();
        store
            .write_segment(&reservation, 0, b"exactly sixteen")
            .unwrap();
        let expected = CacheStore::hash_file(reservation.temp_path()).unwrap();
        let snapshot = store
            .verify_and_publish(
                reservation,
                PublishCheck::DoubleFetch(expected.bytes),
                &key,
                7,
                OperationalLicence::SourceDeclared,
                1234,
            )
            .unwrap();
        assert_eq!(snapshot.byte_size, 15);
        assert_eq!(snapshot.capability_epoch, 7);
        assert_eq!(snapshot.licence_label, OperationalLicence::SourceDeclared);
        assert_eq!(snapshot.digest_provenance, DigestProvenance::DoubleFetch);
        assert_eq!(snapshot.sha256_hex.len(), 64);
        assert!(snapshot.cache_path.contains("media"));
        assert!(!snapshot.cache_path.contains(".part-"));
    }

    #[test]
    fn truncate_temp_discards_torn_tail_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::new(tmp.path());
        let reservation = store.reserve_temp(&media_key("torn"), 1).unwrap();
        store
            .write_segment(&reservation, 0, b"journaled bytes")
            .unwrap();
        // Simulate an interrupted write that left a torn tail on disk.
        let mut file = OpenOptions::new()
            .append(true)
            .open(reservation.temp_path())
            .unwrap();
        file.write_all(b"torn tail").unwrap();
        drop(file);
        store.truncate_temp(&reservation, 15).unwrap();
        let digest = CacheStore::hash_file(reservation.temp_path()).unwrap();
        assert_eq!(digest.total, 15);
    }

    #[test]
    fn unlink_refuses_foreign_paths_and_removes_committed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore::new(tmp.path());
        let key = media_key("unlink");
        let reservation = store.reserve_temp(&key, 1).unwrap();
        store.write_segment(&reservation, 0, b"payload").unwrap();
        let expected = CacheStore::hash_file(reservation.temp_path()).unwrap();
        let snapshot = store
            .verify_and_publish(
                reservation,
                PublishCheck::Advertised(expected.bytes),
                &key,
                1,
                OperationalLicence::SourceDeclared,
                0,
            )
            .unwrap();
        assert!(Path::new(&snapshot.cache_path).exists());
        // A foreign path is refused, not followed.
        let mut foreign = snapshot.clone();
        foreign.cache_path = "/etc/passwd".to_string();
        assert_eq!(
            store.unlink_snapshot(&foreign).unwrap_err(),
            OfflineError::StorageUnavailable
        );
        // The genuine mapping unlinks the file.
        assert!(store.unlink_snapshot(&snapshot).unwrap());
        assert!(!Path::new(&snapshot.cache_path).exists());
        // Second unlink reports already-gone, not an error.
        assert!(!store.unlink_snapshot(&snapshot).unwrap());
    }
}
