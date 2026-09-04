//! The offline cache filesystem boundary.
//!
//! This module is the only place in the offline subsystem that touches the
//! filesystem (`docs/offline-media.md`, "Atomic storage"). It owns:
//!
//! - **Derived cache keys.** `source_key` and `track_key` are the first 32
//!   hex characters of `SHA-256(identifier_bytes)`. The exact, unmodified
//!   byte sequence of each identifier is hashed; nothing is parsed,
//!   normalised, or interpreted, and raw `TrackId` bytes never appear in a
//!   path.
//! - **Temp reservation.** The temp file is created in the same directory
//!   as the final cache path (`<final_name>.part-<job-id>`), so publish is
//!   a same-filesystem rename. A temp that cannot be created beside its
//!   final path fails `StorageUnavailable` at admission; there is no
//!   cross-filesystem publish path.
//! - **The durable journal.** One append-only JSON-lines sidecar per job,
//!   `fsync`'d on every record. The journal — not the temp file's raw
//!   on-disk length — is the only trusted resume state.
//! - **Verify-then-publish.](Nothing in this module renames before the
//!   engine has verified the temp file; [`CacheLayout::publish`] is the
//!   single atomic rename with a parent-directory `fsync` on Unix.
//! - **Content-aware unlink.** Recorded cache paths are structurally
//!   validated against the derived layout before any unlink.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    validate_snapshot_path_bytes, CommittedSnapshot, EntityValidator, MediaKey, OfflineError,
};
use crate::architecture::{SourceId, TrackId};

/// Credential-free constant file name inside a per-track directory. The
/// directory is the per-track scope, so the name carries no identity
/// beyond the recorded mapping (`docs/offline-media.md`, "Per-source
/// layout").
pub const CACHE_FILE_NAME: &str = "media.bin";

/// Byte width of a derived cache key: 128 bits of SHA-256 output, hex
/// encoded to 32 fixed-charset characters.
pub const CACHE_KEY_HEX_LEN: usize = 32;

/// Hex-encode the leading [`CACHE_KEY_HEX_LEN`] characters' worth (16
/// bytes) of `SHA-256(identifier_bytes)` — the one-way cache-key
/// derivation. The identifier is fed to the hash as its exact, unmodified
/// byte sequence; the derivation is not parsing and never feeds back into
/// identity.
pub fn derive_cache_key(identifier_bytes: &[u8]) -> String {
    let digest = Sha256::digest(identifier_bytes);
    let mut key = String::with_capacity(CACHE_KEY_HEX_LEN);
    for byte in digest.iter().take(CACHE_KEY_HEX_LEN / 2) {
        key.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble"));
        key.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("low nibble"));
    }
    key
}

/// Derived cache key for a [`SourceId`]: SHA-256 over the exact UUID bytes.
pub fn source_key(source_id: SourceId) -> String {
    derive_cache_key(source_id.as_uuid().as_bytes())
}

/// Derived cache key for a [`TrackId`]: SHA-256 over the exact, unmodified
/// UTF-8 byte sequence of the opaque identifier.
pub fn track_key(track_id: &TrackId) -> String {
    derive_cache_key(track_id.as_str().as_bytes())
}

/// Deterministic, filesystem-safe slot identifier of one job at one
/// capability epoch: the first 16 hex characters of
/// `SHA-256(source_key || track_key || epoch_le)`. Derived from identity
/// only — never from a URL, credential, or random state — so restart
/// recovery can re-derive temp and journal names without an index.
pub fn job_slot_id(media_key: &MediaKey, capability_epoch: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_key(media_key.source_id).as_bytes());
    hasher.update(track_key(&media_key.track_id).as_bytes());
    hasher.update(capability_epoch.to_le_bytes());
    let digest = hasher.finalize();
    let mut slot = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        slot.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble"));
        slot.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("low nibble"));
    }
    slot
}

/// One durable record of the per-job progress journal. The journal — the
/// job head plus one record per committed segment — is the only trusted
/// resume state; a raw temp length is never trusted
/// (`docs/offline-media.md:193-205`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum JournalRecord {
    /// First record of every journal. Pins the job identity and the
    /// entity validator captured from the first successful response.
    Head {
        media_key: MediaKey,
        capability_epoch: u64,
        validator: Option<EntityValidator>,
    },
    /// One durably committed receive segment: `[offset, offset + len)`
    /// bytes of the temp file, hashed over exactly those bytes. Records
    /// are contiguous and ordered; out-of-order or duplicate ranges are
    /// rejected on load.
    Segment {
        offset: u64,
        len: u64,
        sha256_hex: String,
    },
    /// The snapshot committed after a verified publish. Appended after the
    /// rename succeeds; the cache row exists from this record on.
    Committed { snapshot: CommittedSnapshot },
    /// Terminal job state. `failure` carries the redacted cause only for
    /// `Failed`; `Cancelled` carries none.
    Terminal {
        state: TerminalState,
        failure: Option<OfflineError>,
    },
    /// The committed row was retired: evicted under quota, superseded by
    /// a committed sibling, or licence-revoked at reconciliation.
    Retired { reason: RetirementReason },
}

/// Terminal job states recorded by [`JournalRecord::Terminal`]. A commit
/// is recorded by [`JournalRecord::Committed`] instead — the cache row and
/// the terminal state are one record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TerminalState {
    /// The job failed terminally; `failure` carries the redacted cause.
    Failed,
    /// The job was cancelled decisively by its owner.
    Cancelled,
}

/// Why a committed row was retired. Licence revocation preserves the
/// file; eviction and supersession unlink it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RetirementReason {
    /// Evicted by the quota layer to restore headroom.
    Evicted,
    /// A newer sibling snapshot was committed for the same media key.
    Superseded,
    /// The source's licence was revoked or withdrawn at reconciliation.
    /// The row is retired but the file is preserved.
    LicenceRevoked,
}

/// A segment record accepted by [`load_journal`].
#[derive(Clone, Debug)]
pub struct JournalSegment {
    pub offset: u64,
    pub len: u64,
    pub sha256_hex: String,
}

/// The parsed, validated content of one journal file.
#[derive(Clone, Debug)]
pub struct LoadedJournal {
    pub media_key: MediaKey,
    pub capability_epoch: u64,
    pub validator: Option<EntityValidator>,
    /// Contiguous, in-order committed segments. Empty when progress was
    /// rejected (out-of-order or duplicate ranges) — such progress is
    /// never trusted and the job restarts from zero.
    pub segments: Vec<JournalSegment>,
    pub committed: Option<CommittedSnapshot>,
    pub terminal: Option<(TerminalState, Option<OfflineError>)>,
    /// The recorded retirement reason, if the row was retired after
    /// commit.
    pub retired: Option<RetirementReason>,
}

impl LoadedJournal {
    /// Total journaled byte offset (end of the last contiguous segment).
    pub fn journaled_offset(&self) -> u64 {
        self.segments
            .last()
            .map(|segment| segment.offset + segment.len)
            .unwrap_or(0)
    }
}

/// Outcome of loading one journal file.
#[derive(Debug)]
pub enum LoadedJournalFile {
    /// The journal parsed and validated.
    Valid(Box<LoadedJournal>),
    /// The journal has no parseable head (empty, torn, or corrupt): its
    /// progress is unusable and its temp artifacts are orphans.
    Unusable,
}

/// Per-track artifacts discovered by [`CacheLayout::scan`].
#[derive(Debug)]
pub struct TrackArtifacts {
    /// Absolute path of the `<source_key>/<track_key>` directory.
    pub track_dir: PathBuf,
    /// Journal files present in the directory.
    pub journal_paths: Vec<PathBuf>,
    /// Temp files (`media.bin.part-<slot>`) present in the directory.
    pub temp_paths: Vec<PathBuf>,
    /// Whether the final cache path exists.
    pub final_present: bool,
}

impl TrackArtifacts {
    /// The recorded (root-relative, forward-slash) form of this track's
    /// final cache path.
    pub fn recorded_cache_path(&self) -> String {
        let track = self
            .track_dir
            .components()
            .next_back()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        let source = self
            .track_dir
            .components()
            .nth_back(1)
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        format!("{source}/{track}/{CACHE_FILE_NAME}")
    }
}

/// The cache root and every path derived from it.
#[derive(Clone, Debug)]
pub struct CacheLayout {
    root: PathBuf,
}

impl CacheLayout {
    /// Bind the layout to a cache root. The root is created lazily by
    /// [`CacheLayout::reserve_temp`].
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<cache_root>/<source_key>/<track_key>` — the per-track scope.
    pub fn track_dir(&self, media_key: &MediaKey) -> PathBuf {
        self.root
            .join(source_key(media_key.source_id))
            .join(track_key(&media_key.track_id))
    }

    /// The final cache path of one media key.
    pub fn final_path(&self, media_key: &MediaKey) -> PathBuf {
        self.track_dir(media_key).join(CACHE_FILE_NAME)
    }

    fn slot_stem(media_key: &MediaKey, capability_epoch: u64) -> String {
        format!(
            "{CACHE_FILE_NAME}.part-{}",
            job_slot_id(media_key, capability_epoch)
        )
    }

    /// The temp reservation path of one job: `<final_name>.part-<job-id>`,
    /// in the same directory as the final cache path.
    pub fn temp_path(&self, media_key: &MediaKey, capability_epoch: u64) -> PathBuf {
        self.track_dir(media_key)
            .join(Self::slot_stem(media_key, capability_epoch))
    }

    /// The durable journal path of one job, beside its temp file.
    pub fn journal_path(&self, media_key: &MediaKey, capability_epoch: u64) -> PathBuf {
        let mut name = Self::slot_stem(media_key, capability_epoch);
        name.push_str(".journal");
        self.track_dir(media_key).join(name)
    }

    /// Reserve the temp file: create the per-track directory and open the
    /// temp with truncate. Because the temp is created inside the final
    /// path's directory, publish is structurally same-filesystem; a
    /// refusal here (different filesystem, read-only parent) fails the job
    /// `StorageUnavailable` at admission — there is no cross-filesystem
    /// publish path.
    pub fn reserve_temp(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
    ) -> Result<File, OfflineError> {
        let dir = self.track_dir(media_key);
        fs::create_dir_all(&dir).map_err(|_| OfflineError::StorageUnavailable)?;
        let temp = self.temp_path(media_key, capability_epoch);
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .map_err(|_| OfflineError::StorageUnavailable)
    }

    /// Open the existing temp for append (resume) — never truncate; the
    /// resume path truncates explicitly to the journaled offset only after
    /// journal validation.
    pub fn open_temp_append(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
    ) -> Result<File, OfflineError> {
        OpenOptions::new()
            .append(true)
            .open(self.temp_path(media_key, capability_epoch))
            .map_err(|_| OfflineError::StorageUnavailable)
    }

    /// Current byte length of the temp file, or `None` when absent.
    pub fn temp_len(&self, media_key: &MediaKey, capability_epoch: u64) -> Option<u64> {
        fs::metadata(self.temp_path(media_key, capability_epoch))
            .ok()
            .map(|metadata| metadata.len())
    }

    /// Truncate the temp file to `offset`, discarding torn tail bytes, and
    /// sync it.
    pub fn truncate_temp(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
        offset: u64,
    ) -> Result<(), OfflineError> {
        let file = OpenOptions::new()
            .write(true)
            .open(self.temp_path(media_key, capability_epoch))
            .map_err(|_| OfflineError::StorageUnavailable)?;
        file.set_len(offset)
            .map_err(|_| OfflineError::StorageUnavailable)?;
        file.sync_all()
            .map_err(|_| OfflineError::StorageUnavailable)
    }

    /// `fsync` the temp file (the "Finalize" step: nothing is visible at
    /// the cache path yet).
    ///
    /// The temp is reopened with write access: `FlushFileBuffers` (the
    /// Windows backing of `sync_all`) requires a write handle, so a
    /// read-only open would refuse the sync on Windows while succeeding
    /// on Unix.
    pub fn sync_temp(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
    ) -> Result<(), OfflineError> {
        OpenOptions::new()
            .write(true)
            .open(self.temp_path(media_key, capability_epoch))
            .map_err(|_| OfflineError::StorageUnavailable)?
            .sync_all()
            .map_err(|_| OfflineError::StorageUnavailable)
    }

    /// SHA-256 over the temp file's bytes in `[from, to)`. Returns the
    /// digest of that range. The engine recomputes digests from the bytes
    /// actually on disk — never from received-byte bookkeeping.
    pub fn hash_temp_range(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
        from: u64,
        to: u64,
    ) -> Result<[u8; 32], OfflineError> {
        let mut file = File::open(self.temp_path(media_key, capability_epoch))
            .map_err(|_| OfflineError::StorageUnavailable)?;
        let hasher = hash_file_range(&mut file, from, to)?;
        Ok(hasher.finalize().into())
    }

    /// SHA-256 over the temp file's bytes in `[0, upto)` plus the running
    /// hasher state, so a resumed receive continues the full-file digest
    /// from the journaled offset without re-trusting the raw length.
    pub fn hash_temp_prefix(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
        upto: u64,
    ) -> Result<(Sha256, [u8; 32]), OfflineError> {
        let mut file = File::open(self.temp_path(media_key, capability_epoch))
            .map_err(|_| OfflineError::StorageUnavailable)?;
        let hasher = hash_file_range(&mut file, 0, upto)?;
        let digest: [u8; 32] = hasher.clone().finalize().into();
        Ok((hasher, digest))
    }

    /// Publish by atomic rename: rename the verified temp onto the cache
    /// path and `fsync` the parent directory on Unix so the published name
    /// survives power loss. A failed rename leaves the temp in place for
    /// cleanup and the cache path untouched.
    pub fn publish(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
    ) -> Result<String, OfflineError> {
        let temp = self.temp_path(media_key, capability_epoch);
        let final_path = self.final_path(media_key);
        fs::rename(&temp, &final_path).map_err(|_| OfflineError::StorageUnavailable)?;
        if let Some(parent) = final_path.parent() {
            let _ = File::open(parent).and_then(|dir| dir.sync_all());
        }
        let recorded = self.recorded_form(&final_path);
        validate_snapshot_path_bytes(recorded.len())?;
        Ok(recorded)
    }

    /// The root-relative, forward-slash recorded form of a path inside the
    /// cache root.
    fn recorded_form(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|relative| {
                let parts: Vec<String> = relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().to_string())
                    .collect();
                parts.join("/")
            })
            .unwrap_or_default()
    }

    /// Structurally validate a recorded cache path: bounded, relative to
    /// the root, exactly `<32-hex>/<32-hex>/media.bin`, every component
    /// fixed-charset — no separators, traversal, or raw identity bytes.
    pub fn validate_recorded_path(&self, recorded: &str) -> Result<PathBuf, OfflineError> {
        validate_snapshot_path_bytes(recorded.len())?;
        let components: Vec<&str> = recorded.split('/').collect();
        if components.len() != 3
            || components[0].len() != CACHE_KEY_HEX_LEN
            || components[1].len() != CACHE_KEY_HEX_LEN
            || components[2] != CACHE_FILE_NAME
            || !components[..2].iter().all(|key| {
                key.bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
        {
            return Err(OfflineError::StorageUnavailable);
        }
        let path = self
            .root
            .join(components[0])
            .join(components[1])
            .join(components[2]);
        Ok(path)
    }

    /// Content-aware unlink of a recorded cache path. The path is
    /// structurally validated before the unlink; a missing file is already
    /// the requested end state. Any other refusal fails
    /// `StorageUnavailable` and the caller leaves the row intact — no
    /// half-evicted state.
    pub fn unlink_recorded(&self, recorded: &str) -> Result<(), OfflineError> {
        let path = self.validate_recorded_path(recorded)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(OfflineError::StorageUnavailable),
        }
    }

    /// Unlink one job's temp file. Best-effort: used on cancel and terminal
    /// failure where the temp must leave no half-promoted state.
    pub fn remove_temp(&self, media_key: &MediaKey, capability_epoch: u64) {
        let _ = fs::remove_file(self.temp_path(media_key, capability_epoch));
    }

    /// Remove a job's journal file entirely (used when a retry at the same
    /// `(media_key, capability_epoch)` reuses the slot). Never called for a
    /// journal that recorded a commit.
    pub fn remove_journal(&self, media_key: &MediaKey, capability_epoch: u64) {
        let _ = fs::remove_file(self.journal_path(media_key, capability_epoch));
    }

    /// Append one record to the job's journal and `fsync` it. The append
    /// is durable before the engine treats the record's bytes as progress.
    pub fn append_journal(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
        record: &JournalRecord,
    ) -> Result<(), OfflineError> {
        let path = self.journal_path(media_key, capability_epoch);
        write_journal_record(&path, record)
    }

    /// (Re)create a job journal containing only its head record. Used at
    /// admission and at restart-from-zero. Never called for a journal that
    /// recorded a commit.
    pub fn reset_journal(
        &self,
        media_key: &MediaKey,
        capability_epoch: u64,
        validator: Option<EntityValidator>,
    ) -> Result<(), OfflineError> {
        let dir = self.track_dir(media_key);
        fs::create_dir_all(&dir).map_err(|_| OfflineError::StorageUnavailable)?;
        let path = self.journal_path(media_key, capability_epoch);
        let mut file = File::create(&path).map_err(|_| OfflineError::StorageUnavailable)?;
        let head = JournalRecord::Head {
            media_key: media_key.clone(),
            capability_epoch,
            validator,
        };
        write_record_line(&mut file, &head)?;
        file.sync_all()
            .map_err(|_| OfflineError::StorageUnavailable)
    }

    /// Scan the cache root for per-track artifacts, to depth three
    /// (`<source_key>/<track_key>/<files>`). Missing root yields an empty
    /// scan.
    pub fn scan(&self) -> Vec<TrackArtifacts> {
        let mut tracks: Vec<TrackArtifacts> = Vec::new();
        let Ok(root) = fs::read_dir(&self.root) else {
            return tracks;
        };
        for source_entry in root.flatten() {
            let source_path = source_entry.path();
            if !source_path.is_dir() {
                continue;
            }
            let Ok(source_dirs) = fs::read_dir(&source_path) else {
                continue;
            };
            for track_entry in source_dirs.flatten() {
                let track_dir = track_entry.path();
                if track_dir.is_dir() {
                    tracks.push(scan_track_dir(track_dir));
                }
            }
        }
        tracks
    }

    /// The recorded cache path of one job's final file.
    pub fn recorded_cache_path(media_key: &MediaKey) -> String {
        format!(
            "{}/{}/{}",
            source_key(media_key.source_id),
            track_key(&media_key.track_id),
            CACHE_FILE_NAME
        )
    }
}

fn write_record_line(file: &mut File, record: &JournalRecord) -> Result<(), OfflineError> {
    let mut line = serde_json::to_string(record)
        .map_err(|_| OfflineError::StorageUnavailable)?
        .into_bytes();
    line.push(b'\n');
    file.write_all(&line)
        .map_err(|_| OfflineError::StorageUnavailable)
}

/// Classify one file entry of a track directory and record it in the
/// artifact set: the final cache name, a `.part-` journal sidecar, or a
/// `.part-` temp file.
fn scan_track_file(artifacts: &mut TrackArtifacts, path: &Path, name: &str) {
    if name == CACHE_FILE_NAME {
        artifacts.final_present = true;
    } else if let Some(stem) = name.strip_suffix(".journal") {
        if stem.contains(".part-") {
            artifacts.journal_paths.push(path.to_path_buf());
        }
    } else if name.contains(".part-") {
        artifacts.temp_paths.push(path.to_path_buf());
    }
}

/// Collect the artifacts of one `<source_key>/<track_key>` directory.
/// An unreadable directory yields an empty artifact set.
fn scan_track_dir(track_dir: PathBuf) -> TrackArtifacts {
    let mut artifacts = TrackArtifacts {
        track_dir,
        journal_paths: Vec::new(),
        temp_paths: Vec::new(),
        final_present: false,
    };
    let Ok(files) = fs::read_dir(&artifacts.track_dir) else {
        return artifacts;
    };
    for file_entry in files.flatten() {
        let path = file_entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            scan_track_file(&mut artifacts, &path, name);
        }
    }
    artifacts
}

/// Hash `[from, to)` of an already-open file, leaving the running hasher
/// with the caller.
fn hash_file_range(file: &mut File, from: u64, to: u64) -> Result<Sha256, OfflineError> {
    std::io::Seek::seek(file, std::io::SeekFrom::Start(from))
        .map_err(|_| OfflineError::StorageUnavailable)?;
    let mut hasher = Sha256::new();
    let mut remaining = to.saturating_sub(from);
    let mut buffer = [0u8; 16 * 1024];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = std::io::Read::read(file, &mut buffer[..want])
            .map_err(|_| OfflineError::StorageUnavailable)?;
        if read == 0 {
            return Err(OfflineError::StorageUnavailable);
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher)
}

fn write_journal_record(path: &Path, record: &JournalRecord) -> Result<(), OfflineError> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|_| OfflineError::StorageUnavailable)?;
    write_record_line(&mut file, record)?;
    file.sync_all()
        .map_err(|_| OfflineError::StorageUnavailable)
}

/// Mutable parse state accumulated across one journal file's records.
struct JournalParse {
    head: Option<(MediaKey, u64, Option<EntityValidator>)>,
    segments: Vec<JournalSegment>,
    progress_trusted: bool,
    committed: Option<CommittedSnapshot>,
    terminal: Option<(TerminalState, Option<OfflineError>)>,
    retired: Option<RetirementReason>,
}

impl Default for JournalParse {
    fn default() -> Self {
        Self {
            head: None,
            segments: Vec::new(),
            progress_trusted: true,
            committed: None,
            terminal: None,
            retired: None,
        }
    }
}

/// Outcome of folding one parsed record into [`JournalParse`].
enum FoldOutcome {
    /// Record accepted; continue with the next line.
    Continue,
    /// The journal is structurally invalid (duplicate head, or a
    /// segment with no head): the whole file is unusable.
    Unusable,
}

/// Fold one validated record into the parse state. Segment ranges must
/// be contiguous and in order: the first violation rejects the whole
/// progress (never trusted) but keeps the file loadable.
fn fold_record(parse: &mut JournalParse, record: JournalRecord) -> FoldOutcome {
    match record {
        JournalRecord::Head {
            media_key,
            capability_epoch,
            validator,
        } => {
            if parse.head.is_some() {
                return FoldOutcome::Unusable;
            }
            parse.head = Some((media_key, capability_epoch, validator));
        }
        JournalRecord::Segment {
            offset,
            len,
            sha256_hex,
        } => {
            let Some(_) = parse.head else {
                return FoldOutcome::Unusable;
            };
            let expected_offset = parse
                .segments
                .last()
                .map(|segment| segment.offset + segment.len)
                .unwrap_or(0);
            if len == 0 || offset != expected_offset {
                // Out-of-order or duplicate ranges are rejected: the
                // journaled progress is untrusted and the job restarts
                // from zero.
                parse.progress_trusted = false;
                parse.segments.clear();
            } else {
                parse.segments.push(JournalSegment {
                    offset,
                    len,
                    sha256_hex,
                });
            }
        }
        JournalRecord::Committed { snapshot } => parse.committed = Some(snapshot),
        JournalRecord::Terminal { state, failure } => parse.terminal = Some((state, failure)),
        JournalRecord::Retired { reason } => parse.retired = Some(reason),
    }
    FoldOutcome::Continue
}

/// Build the [`LoadedJournal`] from the folded state. A file with no
/// parseable head has no identity: unusable.
fn finish_journal(parse: JournalParse) -> LoadedJournalFile {
    let Some((media_key, capability_epoch, validator)) = parse.head else {
        return LoadedJournalFile::Unusable;
    };
    LoadedJournalFile::Valid(Box::new(LoadedJournal {
        media_key,
        capability_epoch,
        validator: if parse.progress_trusted {
            validator
        } else {
            None
        },
        segments: parse.segments,
        committed: parse.committed,
        terminal: parse.terminal,
        retired: parse.retired,
    }))
}

/// Parse and validate one journal file. A torn final line — an
/// interrupted append — is discarded; a corrupt mid-file line makes the
/// journal unusable.
pub fn load_journal(path: &Path) -> LoadedJournalFile {
    let Ok(bytes) = fs::read(path) else {
        return LoadedJournalFile::Unusable;
    };
    let chunks: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();
    let last = chunks.len().saturating_sub(1);
    let mut parse = JournalParse::default();
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.is_empty() && index == last {
            break;
        }
        let Ok(line) = std::str::from_utf8(chunk) else {
            if index == last {
                break; // torn tail from an interrupted append
            }
            return LoadedJournalFile::Unusable;
        };
        let record: JournalRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(_) => {
                if index == last {
                    break; // torn tail
                }
                return LoadedJournalFile::Unusable;
            }
        };
        if matches!(fold_record(&mut parse, record), FoldOutcome::Unusable) {
            return LoadedJournalFile::Unusable;
        }
    }
    finish_journal(parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str, track: &str) -> MediaKey {
        MediaKey::new(
            SourceId::local(),
            TrackId::new(format!("{source}-{track}")).expect("track id"),
        )
    }

    #[test]
    fn derived_cache_keys_are_fixed_width_hex_and_input_sensitive() {
        let first = derive_cache_key(b"track-one");
        let second = derive_cache_key(b"track-two");
        assert_eq!(first.len(), CACHE_KEY_HEX_LEN);
        assert_eq!(second.len(), CACHE_KEY_HEX_LEN);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_ne!(first, second);
        // Stable across invocations and independent of runtime state.
        assert_eq!(first, derive_cache_key(b"track-one"));
    }

    #[test]
    fn derived_keys_never_leak_raw_identifier_bytes_into_paths() {
        // A raw TrackId may contain separators, traversal, unicode, and
        // control characters; the derived path components may not.
        let hostile = TrackId::new("../../etc/passwd☃\u{0}").expect("bounded track id");
        let key = track_key(&hostile);
        assert_eq!(key.len(), CACHE_KEY_HEX_LEN);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!key.contains(".."));
        assert!(!key.contains('/'));
    }

    #[test]
    fn job_slot_id_is_deterministic_in_key_and_epoch() {
        let media_key = key("a", "1");
        assert_eq!(job_slot_id(&media_key, 1), job_slot_id(&media_key, 1));
        assert_ne!(job_slot_id(&media_key, 1), job_slot_id(&media_key, 2));
        let other = key("a", "2");
        assert_ne!(job_slot_id(&media_key, 1), job_slot_id(&other, 1));
        let slot = job_slot_id(&media_key, 1);
        assert_eq!(slot.len(), 16);
        assert!(slot.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn temp_is_reserved_in_the_final_directory() {
        let temp_dir = tempfile::tempdir().expect("temp root");
        let layout = CacheLayout::new(temp_dir.path());
        let media_key = key("s", "t");
        layout
            .reserve_temp(&media_key, 1)
            .expect("temp reservation");
        let temp = layout.temp_path(&media_key, 1);
        assert!(temp.exists());
        assert_eq!(
            temp.parent(),
            layout.final_path(&media_key).parent(),
            "temp must share the final path's directory"
        );
        assert!(temp
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(CACHE_FILE_NAME) && name.contains(".part-")));
    }

    #[test]
    fn recorded_paths_validate_structurally_and_reject_traversal() {
        let temp_dir = tempfile::tempdir().expect("temp root");
        let layout = CacheLayout::new(temp_dir.path());
        let media_key = key("s", "t");
        let recorded = CacheLayout::recorded_cache_path(&media_key);
        assert!(layout.validate_recorded_path(&recorded).is_ok());

        for hostile in [
            "../escape/media.bin",
            "gg/../media.bin",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/media.bin",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/other.bin",
            "",
            "0123456789abcdef0123456789abcdef0123456789abcdef/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/../../media.bin",
        ] {
            assert!(
                layout.validate_recorded_path(hostile).is_err(),
                "{hostile} must fail structural validation"
            );
        }
        assert!(layout.unlink_recorded(&recorded).is_ok()); // missing = done
    }

    #[test]
    fn publish_renames_atomically_and_leaves_the_temp_gone() {
        let temp_dir = tempfile::tempdir().expect("temp root");
        let layout = CacheLayout::new(temp_dir.path());
        let media_key = key("s", "t");
        {
            let mut file = layout.reserve_temp(&media_key, 1).expect("reserve");
            std::io::Write::write_all(&mut file, b"payload").expect("write");
        }
        let recorded = layout.publish(&media_key, 1).expect("publish");
        assert_eq!(recorded, CacheLayout::recorded_cache_path(&media_key));
        assert!(!layout.temp_path(&media_key, 1).exists());
        assert!(layout.final_path(&media_key).exists());
        let bytes = fs::read(layout.final_path(&media_key)).expect("read published");
        assert_eq!(bytes, b"payload");
    }

    #[test]
    fn journal_round_trips_and_rejects_disordered_segments() {
        let temp_dir = tempfile::tempdir().expect("temp root");
        let layout = CacheLayout::new(temp_dir.path());
        let media_key = key("s", "t");
        layout
            .reset_journal(&media_key, 3, Some(EntityValidator::ETag("\"v1\"".into())))
            .expect("reset");
        layout
            .append_journal(
                &media_key,
                3,
                &JournalRecord::Segment {
                    offset: 0,
                    len: 10,
                    sha256_hex: "aa".repeat(32),
                },
            )
            .expect("append");
        layout
            .append_journal(
                &media_key,
                3,
                &JournalRecord::Segment {
                    offset: 10,
                    len: 5,
                    sha256_hex: "bb".repeat(32),
                },
            )
            .expect("append");

        let journal_path = layout.journal_path(&media_key, 3);
        let LoadedJournalFile::Valid(journal) = load_journal(&journal_path) else {
            panic!("journal must load");
        };
        assert_eq!(journal.capability_epoch, 3);
        assert_eq!(journal.segments.len(), 2);
        assert_eq!(journal.journaled_offset(), 15);

        // An out-of-order segment rejects the whole progress.
        layout
            .append_journal(
                &media_key,
                3,
                &JournalRecord::Segment {
                    offset: 4,
                    len: 5,
                    sha256_hex: "cc".repeat(32),
                },
            )
            .expect("append");
        let LoadedJournalFile::Valid(journal) = load_journal(&journal_path) else {
            panic!("journal must still load");
        };
        assert!(
            journal.segments.is_empty(),
            "disordered progress is untrusted"
        );
        assert!(journal.validator.is_none());
        assert_eq!(journal.journaled_offset(), 0);
    }

    #[test]
    fn torn_journal_tail_is_discarded_but_mid_file_corruption_is_not() {
        let temp_dir = tempfile::tempdir().expect("temp root");
        let layout = CacheLayout::new(temp_dir.path());
        let media_key = key("s", "t");
        layout.reset_journal(&media_key, 1, None).expect("reset");
        let journal_path = layout.journal_path(&media_key, 1);

        // Torn final append (half a line) is discarded on load.
        let mut file = OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open");
        std::io::Write::write_all(&mut file, br#"{"Segm"#).expect("torn append");
        drop(file);
        let LoadedJournalFile::Valid(journal) = load_journal(&journal_path) else {
            panic!("torn tail must not invalidate the journal");
        };
        assert_eq!(journal.capability_epoch, 1);

        // Corrupt full mid-file line makes the journal unusable.
        let mut file = OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open");
        std::io::Write::write_all(&mut file, b"not-json\n").expect("corrupt line");
        drop(file);
        assert!(matches!(
            load_journal(&journal_path),
            LoadedJournalFile::Unusable
        ));
    }

    #[test]
    fn scan_discovers_track_artifacts_by_shape() {
        let temp_dir = tempfile::tempdir().expect("temp root");
        let layout = CacheLayout::new(temp_dir.path());
        let media_key = key("s", "t");
        layout.reserve_temp(&media_key, 1).expect("reserve");
        layout
            .reset_journal(&media_key, 1, None)
            .expect("reset journal");

        let tracks = layout.scan();
        assert_eq!(tracks.len(), 1);
        let track = &tracks[0];
        assert_eq!(track.temp_paths.len(), 1);
        assert_eq!(track.journal_paths.len(), 1);
        assert!(!track.final_present);
        assert_eq!(
            track.recorded_cache_path(),
            CacheLayout::recorded_cache_path(&media_key)
        );
    }
}
