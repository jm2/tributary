//! The bounded download/cache engine.
//!
//! This module is the only place that orchestrates jobs
//! (`docs/offline-media.md`, "Authenticated, resumable download jobs"). It
//! owns one [`JobRecord`] per `(media_key, capability_epoch)`, drives each
//! job through `Queued → Connecting → Receiving → Verifying → Committing →
//! Committed` (or a terminal `Failed`/`Cancelled`), and delegates every
//! byte of filesystem work to [`super::storage`] and every quota/eviction
//! decision to [`super::quota`].
//!
//! The engine never becomes a credential owner: bytes arrive through an
//! [`OfflineTransport`] supplied by the supervisor — the same
//! exact-origin boundary live playback uses — and the engine persists only
//! redacted, structured state. Durable job state is the per-job journal;
//! nothing is memory-only, and [`OfflineEngine::open`] re-derives
//! `Queued`/`Receiving` state from the journals on restart.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::catalog::CatalogueIndex;
use super::quota::{self, AdmissionVerdict};
use super::storage::{
    self, CacheLayout, JournalRecord, LoadedJournalFile, RetirementReason, TrackArtifacts,
};
use super::{
    validate_byte_hint, CommittedSnapshot, DigestProvenance, EntityValidator, JobRecord, JobState,
    LeaseId, OfflineCatalogueEntry, OfflineError, OfflineSnapshot, OperationalLicence,
};
use crate::architecture::{MediaKey, SourceId, TrackId};

/// Engine tuning. The global quota is the single application-wide bound;
/// per-source caps stay advisory at admission.
#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    /// Global offline quota in bytes across every committed row.
    pub global_quota_bytes: u64,
    /// Receive granularity: one journal segment per this many received
    /// bytes. Every segment is `fsync`'d to the temp file and journaled
    /// before its bytes count as progress.
    pub segment_bytes: usize,
    /// How many transient [`OfflineError::Network`] pauses one job may
    /// absorb before the failure turns terminal.
    pub resume_budget: u32,
    /// Epoch-seconds source for committed-at stamps. Injectable so tests
    /// can drive deterministic eviction order.
    pub clock: fn() -> u64,
}

impl EngineConfig {
    /// Configuration with the default segment size and resume budget.
    pub fn new(global_quota_bytes: u64) -> Self {
        Self {
            global_quota_bytes,
            ..Self::default()
        }
    }

    /// Deterministic clock for tests: each call advances one second.
    pub fn counting_clock() -> fn() -> u64 {
        // Returns monotonically increasing seconds per process-wide call;
        // only relative order matters to the tests.
        counting_clock_next
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            global_quota_bytes: 64 * 1024 * 1024,
            segment_bytes: 64 * 1024,
            resume_budget: 3,
            clock: system_epoch_secs,
        }
    }
}

/// Epoch seconds from the system clock; 0 when the clock is before the
/// Unix epoch (never in practice, and only a stamp).
pub fn system_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

fn counting_clock_next() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(1);
    COUNT.fetch_add(1, Ordering::Relaxed)
}

/// Engine-facing view of one opened transfer's response head.
#[derive(Clone, Debug)]
pub struct TransferHead {
    /// `Partial` only for a validated range continuation (`206`);
    /// `Fresh` covers a full entity (`200`) and a failed validator
    /// (`412`) — both discard partial bytes and restart from zero.
    pub outcome: TransferOutcome,
    /// Full-entity length when the source advertises one. Validated
    /// against the [`super::MAX_OFFLINE_BYTE_HINT`] per-file media
    /// ceiling before any byte is received.
    pub total_length: Option<u64>,
    /// Entity validator captured from this response. `None` disables
    /// resume for the job.
    pub validator: Option<EntityValidator>,
    /// Tier-1 expected digest (the adapter's advertised provenance);
    /// `None` routes verification through double-fetch.
    pub advertised_sha256: Option<[u8; 32]>,
    /// Opaque lease of the in-flight request, as minted by the source
    /// registry. The engine treats it as identity only.
    pub lease: Option<LeaseId>,
}

/// Whether an opened transfer continues a validated range or starts fresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferOutcome {
    /// `206`: the resume validator held; the stream continues from the
    /// requested offset.
    Partial,
    /// `200`/`412`: the entity changed or was never validated; partial
    /// bytes are discarded and the job restarts from zero.
    Fresh,
}

/// One authenticated transfer request. The transport owns every URL,
/// credential, and header; the engine never sees them.
#[derive(Clone, Copy, Debug)]
pub struct TransferRequest<'a> {
    pub media_key: &'a MediaKey,
    /// Resume offset for a validated range request; `None` for a full
    /// fetch.
    pub resume_from: Option<u64>,
    /// The captured entity validator sent as `If-Range` on a resume.
    pub if_range: Option<&'a EntityValidator>,
}

/// The engine's only network boundary. Implementations supply the
/// authenticated, redirect-policy-compliant byte path through the existing
/// exact-origin proxy; the engine sees opaque bytes and redacted errors.
pub trait OfflineTransport {
    /// Open a transfer for the request. Errors map to the redacted
    /// [`OfflineError`] variants (`Network`, `AuthExpired`,
    /// `LeaseRevoked`, `StorageUnavailable`).
    fn open(&mut self, request: TransferRequest<'_>) -> Result<TransferHead, OfflineError>;
    /// Read the next chunk of the open transfer; `Ok(0)` is end of
    /// stream.
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, OfflineError>;
    /// Whether a fresh second transfer can be issued for double-fetch
    /// verification.
    fn double_fetch_supported(&self) -> bool;
    /// Revoke the in-flight lease. Idempotent; the engine calls it on
    /// every exit from a drive so no lease outlives a job step.
    fn close(&mut self);
}

/// Result of [`OfflineEngine::admit`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// A new job started; drive it with [`OfflineEngine::drive`].
    Started,
    /// A non-terminal job already owns this `(media_key,
    /// capability_epoch)`; the newer request waits for its terminal state.
    InFlight,
    /// A verified snapshot for this exact capability epoch is already
    /// committed. Same-epoch content is identical by definition, so
    /// re-downloading it is refused; a refresh takes a newer epoch.
    Current,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
struct JobKey(SourceId, TrackId);

fn job_key(media_key: &MediaKey) -> JobKey {
    JobKey(media_key.source_id, media_key.track_id.clone())
}

/// State carried out of [`OfflineEngine::open_transfer`] into the
/// receive loop: the running full-file digest, the trusted received
/// offset, the advertised length, and the advertised digest.
struct OpenTransfer {
    hasher: Sha256,
    received: u64,
    total_length: Option<u64>,
    advertised: Option<[u8; 32]>,
}

/// Receive-loop bookkeeping for the segment currently being
/// accumulated: its running digest and its `[start, start + len)`
/// byte range within the temp file.
struct SegmentProgress {
    hasher: Sha256,
    start: u64,
    len: u64,
}

impl SegmentProgress {
    /// Begin a new segment at the current received offset.
    fn new(received: u64) -> Self {
        Self {
            hasher: Sha256::new(),
            start: received,
            len: 0,
        }
    }

    /// Close the finished segment and begin the next one at `received`.
    fn reset(&mut self, received: u64) {
        self.start = received;
        self.len = 0;
        self.hasher = Sha256::new();
    }
}

/// One journal file recovered from disk during restart recovery, with
/// the facts recovery needs to rebuild rows and the live job.
struct RecoveredJob {
    record: JobRecord,
    committed: Option<CommittedSnapshot>,
    retired: Option<RetirementReason>,
    validator: Option<EntityValidator>,
    journaled_offset: u64,
    has_segments: bool,
}

/// The download/cache engine. Not `Clone`: it owns the durable cache
/// state under its root. Re-open the same root to recover.
pub struct OfflineEngine {
    layout: CacheLayout,
    config: EngineConfig,
    index: CatalogueIndex,
    jobs: HashMap<JobKey, JobRecord>,
    cancel_requested: HashSet<JobKey>,
    network_pauses: HashMap<JobKey, u32>,
}

impl OfflineEngine {
    /// Open (or recover) the engine over a cache root. Restart recovery
    /// re-derives job and catalogue state from the durable journals,
    /// truncates torn temp tails to the journaled offset, rejects
    /// untrusted progress, and unlinks orphan artifacts that no recorded
    /// mapping owns.
    #[allow(clippy::unnecessary_wraps)] // the SQLite-backed successor fails fallibly
    pub fn open(root: impl Into<PathBuf>, config: EngineConfig) -> Result<Self, OfflineError> {
        let mut engine = Self {
            layout: CacheLayout::new(root),
            config,
            index: CatalogueIndex::new(),
            jobs: HashMap::new(),
            cancel_requested: HashSet::new(),
            network_pauses: HashMap::new(),
        };
        engine.recover();
        Ok(engine)
    }

    /// The cache layout backing this engine (supervisor introspection
    /// only — callers never construct cache paths from identifiers
    /// outside the recorded mapping).
    pub fn layout(&self) -> &CacheLayout {
        &self.layout
    }

    /// The committed-row index (read path).
    pub fn catalogue_index(&self) -> &CatalogueIndex {
        &self.index
    }

    /// Resolve the offline catalogue read for one media key.
    pub fn catalogue(&self, media_key: &MediaKey) -> OfflineCatalogueEntry {
        self.index.resolve(media_key)
    }

    /// The recorded snapshots of one source (revoked included).
    pub fn source_snapshots(&self, source_id: SourceId) -> Vec<CommittedSnapshot> {
        self.index.source_snapshots(source_id)
    }

    /// The current job record for one media key, if any.
    pub fn job(&self, media_key: &MediaKey) -> Option<&JobRecord> {
        self.jobs.get(&job_key(media_key))
    }

    /// Total committed bytes (revoked rows included — their files are
    /// preserved and still occupy the quota).
    pub fn total_committed_bytes(&self) -> u64 {
        self.index.total_committed_bytes()
    }

    /// The admission decisions that need no network and no quota: a
    /// non-terminal job for the key waits (`InFlight`), and an already
    /// committed exact capability epoch short-circuits (`Current`).
    fn admission_short_circuit(
        &self,
        media_key: &MediaKey,
        key: &JobKey,
        capability_epoch: u64,
    ) -> Option<Admission> {
        let job = self.jobs.get(key)?;
        if !job.state.is_terminal() {
            return Some(Admission::InFlight);
        }
        if job.capability_epoch == capability_epoch
            && job.state == JobState::Committed
            && !matches!(
                self.index.resolve(media_key),
                OfflineCatalogueEntry::LiveOnly
            )
        {
            // This exact version is already committed: same epoch
            // means identical content, so re-downloading is refused.
            return Some(Admission::Current);
        }
        None
    }

    /// Admit one download job. All gates run before any network work:
    /// capability declaration, operational licence, one-job-per-key, the
    /// advisory per-source cap, and global quota headroom (with a
    /// best-effort eviction pass when the quota is already full).
    pub fn admit(
        &mut self,
        media_key: &MediaKey,
        capability_epoch: u64,
        capability: &Result<Option<OfflineSnapshot>, OfflineError>,
        licence: OperationalLicence,
    ) -> Result<Admission, OfflineError> {
        let key = job_key(media_key);
        if let Some(short) = self.admission_short_circuit(media_key, &key, capability_epoch) {
            return Ok(short);
        }

        // Restore headroom first so a full quota evicts instead of
        // refusing when eviction can make room.
        if self.index.total_committed_bytes() >= self.config.global_quota_bytes {
            self.evict_to_quota()?;
        }
        let verdict = quota::admission_verdict(
            capability,
            licence,
            self.index.source_committed_bytes(media_key.source_id),
            self.index.total_committed_bytes(),
            self.config.global_quota_bytes,
        );
        match verdict {
            AdmissionVerdict::Admitted => {}
            AdmissionVerdict::UnsupportedSource => {
                return Err(OfflineError::UnsupportedSource);
            }
            AdmissionVerdict::LicenceDenied => return Err(OfflineError::LicenceDenied),
            AdmissionVerdict::SourceCapExhausted | AdmissionVerdict::QuotaExceeded => {
                return Err(OfflineError::QuotaExceeded);
            }
        }

        // A terminal predecessor at this slot is retryable: its journal
        // recorded no commit. A committed journal at a *different* epoch
        // is never touched — that would erase a durable cache row.
        self.layout.remove_journal(media_key, capability_epoch);
        self.layout.remove_temp(media_key, capability_epoch);
        self.layout
            .reset_journal(media_key, capability_epoch, None)?;
        self.jobs.insert(
            key.clone(),
            JobRecord::new(media_key.clone(), capability_epoch),
        );
        self.network_pauses.remove(&key);
        self.cancel_requested.remove(&key);
        Ok(Admission::Started)
    }

    /// Drive the job for one media key toward a terminal state. Returns
    /// `None` when no job is known for the key. Every exit path closes
    /// the transport, so no lease outlives the drive.
    ///
    /// Failures are recorded on the job (redacted [`OfflineError`]); a
    /// transient `Network` error pauses the job inside its resume budget
    /// and stays resumable from the journal.
    pub fn drive(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
    ) -> Option<JobState> {
        let key = job_key(media_key);
        let media_key = media_key.clone();
        let job = self.jobs.get(&key)?;
        if job.state.is_terminal() {
            return Some(job.state);
        }
        let epoch = job.capability_epoch;
        let open = match self.open_transfer(transport, &media_key, epoch) {
            Ok(open) => open,
            Err(state) => return Some(state),
        };
        let advertised = open.advertised;
        let (_, received) = match self.receive(transport, &media_key, epoch, open) {
            Ok(done) => done,
            Err(state) => return Some(state),
        };
        let (on_disk, provenance) =
            match self.verify_transfer(transport, &media_key, epoch, received, advertised) {
                Ok(verified) => verified,
                Err(state) => return Some(state),
            };
        Some(self.commit_snapshot(transport, &media_key, epoch, received, on_disk, provenance))
    }

    /// Phase 1: validate the resume basis, open the transfer, and decide
    /// whether this drive continues from the journaled offset or
    /// restarts from zero. `Err` carries the job's terminal state.
    fn open_transfer(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
    ) -> Result<OpenTransfer, JobState> {
        let key = job_key(media_key);
        let mut journaled = self.prepare_resume(transport, media_key, epoch)?;
        let validator = self
            .jobs
            .get(&key)
            .and_then(|job| job.resume_validator.clone());
        if journaled > 0 && validator.is_none() {
            // `Some` validator is required for any resumption: a job that
            // captured none restarts fully.
            self.restart_from_zero(media_key, epoch);
            journaled = 0;
        }
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Connecting;
        }
        let opened = transport.open(TransferRequest {
            media_key,
            resume_from: (journaled > 0).then_some(journaled),
            if_range: (journaled > 0).then_some(validator.as_ref()).flatten(),
        });
        let head = match opened {
            Ok(head) => head,
            Err(error) => {
                let state = self.on_transport_error(transport, media_key, error);
                return Err(state);
            }
        };
        if let Some(job) = self.jobs.get_mut(&key) {
            job.last_lease = head.lease;
            job.requested_bytes = head.total_length;
        }
        let (hasher, received) =
            self.seed_receive_basis(transport, media_key, epoch, journaled, &head)?;
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Receiving;
        }
        Ok(OpenTransfer {
            hasher,
            received,
            total_length: head.total_length,
            advertised: head.advertised_sha256,
        })
    }

    /// Phase 1a: validate the resume basis and prepare the temp for
    /// this drive. Returns the trusted journaled offset — `0` for a
    /// fresh receive or when untrusted progress was discarded. `Err`
    /// carries the job's terminal state.
    fn prepare_resume(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
    ) -> Result<u64, JobState> {
        let key = job_key(media_key);
        let mut journaled = self
            .jobs
            .get(&key)
            .map(|job| job.current_bytes)
            .unwrap_or(0);
        if journaled > 0 {
            match self.verify_resume_basis(media_key, epoch, journaled) {
                Ok(true) => {}
                Ok(false) => {
                    // Untrusted progress (no segments, no validator, or a
                    // last-segment digest that does not match the bytes on
                    // disk): discard it and restart from zero.
                    self.restart_from_zero(media_key, epoch);
                    journaled = 0;
                }
                Err(error) => {
                    self.finalize_failed(media_key, epoch, error, true);
                    transport.close();
                    return Err(JobState::Failed);
                }
            }
        }
        if journaled == 0 {
            // Fresh receive: reserve the temp beside the final path —
            // same-directory by construction, so a refusal here is the
            // cross-filesystem/read-only admission failure.
            if self.layout.reserve_temp(media_key, epoch).is_err() {
                self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
                transport.close();
                return Err(JobState::Failed);
            }
        }
        Ok(journaled)
    }

    /// Phase 1b: decide from the opened transfer whether this drive
    /// continues from the journaled offset (a held `206` validator) or
    /// restarts from zero (a fresh or stale `200`/`412` entity), seeding
    /// the running digest from the bytes on disk in the continuing case
    /// and capturing the fresh entity validator otherwise. The
    /// advertised length is validated against the byte-hint ceiling
    /// before any byte is received. `Err` carries the terminal state.
    fn seed_receive_basis(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
        journaled: u64,
        head: &TransferHead,
    ) -> Result<(Sha256, u64), JobState> {
        let continuing = journaled > 0 && head.outcome == TransferOutcome::Partial;
        if continuing {
            // 206: the validator held — continue from the journaled
            // offset. The running digest is seeded from the bytes on
            // disk, never from bookkeeping.
            match self.layout.hash_temp_prefix(media_key, epoch, journaled) {
                Ok((running, _)) => return Ok((running, journaled)),
                Err(_) => {
                    self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
                    transport.close();
                    return Err(JobState::Failed);
                }
            }
        }
        // 200/412 on a resume (entity changed or never validated), or
        // a plain fresh fetch: partial bytes are discarded and the
        // job restarts from zero under the same job ID, on this
        // transfer.
        if journaled > 0 {
            self.restart_from_zero(media_key, epoch);
        }
        self.capture_validator(media_key, epoch, head.validator.clone());
        if let Some(total) = head.total_length {
            if validate_byte_hint(total).is_err() {
                // A hint the cache layer cannot trust is malformed
                // input, not a quota overrun.
                self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
                transport.close();
                return Err(JobState::Failed);
            }
        }
        Ok((Sha256::new(), 0))
    }

    /// Phase 2: receive the open transfer, journaling every committed
    /// segment before its bytes count as progress. `Ok` carries the
    /// final running digest and the trusted received total; `Err`
    /// carries the job's terminal state (`Cancelled`, or `Failed` after
    /// a storage or quota refusal).
    fn receive(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
        open: OpenTransfer,
    ) -> Result<(Sha256, u64), JobState> {
        let key = job_key(media_key);
        let OpenTransfer {
            mut hasher,
            mut received,
            total_length,
            advertised: _,
        } = open;
        let Ok(mut file) = self.layout.open_temp_append(media_key, epoch) else {
            self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
            transport.close();
            return Err(JobState::Failed);
        };
        let chunk = self.config.segment_bytes.clamp(1, 64 * 1024);
        let mut buffer = vec![0u8; chunk];
        let mut segment = SegmentProgress::new(received);
        loop {
            if self.cancel_requested.contains(&key) {
                // Cancellation is decisive: revoke the in-flight lease
                // promptly, then record the terminal state.
                transport.close();
                self.finalize_cancelled(media_key, epoch);
                return Err(self.current_job_state(&key));
            }
            let read = match transport.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    let state = self.on_transport_error(transport, media_key, error);
                    return Err(state);
                }
            };
            let end_of_stream = read == 0;
            if !end_of_stream {
                if let Some(total) = total_length {
                    if received + read as u64 > total {
                        // The source sent beyond its advertised length: a
                        // byte count the cache layer cannot trust.
                        self.finalize_failed(
                            media_key,
                            epoch,
                            OfflineError::StorageUnavailable,
                            true,
                        );
                        transport.close();
                        return Err(JobState::Failed);
                    }
                }
                let used = self.global_used_excluding(&key) + received;
                if quota::receive_overruns_quota(used, read as u64, self.config.global_quota_bytes)
                {
                    self.finalize_failed(media_key, epoch, OfflineError::QuotaExceeded, true);
                    transport.close();
                    return Err(JobState::Failed);
                }
                if file.write_all(&buffer[..read]).is_err() {
                    self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
                    transport.close();
                    return Err(JobState::Failed);
                }
                hasher.update(&buffer[..read]);
                segment.hasher.update(&buffer[..read]);
                received += read as u64;
                segment.len += read as u64;
            }

            // Commit a segment at the configured boundary or at end of
            // stream — durably: fsync the temp bytes, journal the segment,
            // and only then advance the trusted offset.
            let boundary = segment.len >= self.config.segment_bytes as u64;
            if end_of_stream || boundary {
                if segment.len > 0 {
                    if let Err(state) = self.journal_receive_segment(
                        media_key,
                        epoch,
                        &file,
                        &mut segment,
                        received,
                    ) {
                        transport.close();
                        return Err(state);
                    }
                }
                if end_of_stream {
                    return Ok((hasher, received));
                }
            }
        }
    }

    /// Durably commit one received segment: `fsync` the temp bytes,
    /// journal the segment, advance the job's trusted offset, and reset
    /// the segment accumulator. `Err` carries the terminal state; the
    /// caller closes the transport.
    fn journal_receive_segment(
        &mut self,
        media_key: &MediaKey,
        epoch: u64,
        file: &File,
        segment: &mut SegmentProgress,
        received: u64,
    ) -> Result<(), JobState> {
        if file.sync_all().is_err() {
            self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
            return Err(JobState::Failed);
        }
        let segment_digest: [u8; 32] = segment.hasher.clone().finalize().into();
        let record = JournalRecord::Segment {
            offset: segment.start,
            len: segment.len,
            sha256_hex: hex(&segment_digest),
        };
        if self
            .layout
            .append_journal(media_key, epoch, &record)
            .is_err()
        {
            self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
            return Err(JobState::Failed);
        }
        let key = job_key(media_key);
        if let Some(job) = self.jobs.get_mut(&key) {
            job.current_bytes = received;
        }
        segment.reset(received);
        Ok(())
    }

    /// Phases 3 and 4: `fsync` the fully received temp file, then verify
    /// it — the SHA-256 is recomputed from the bytes actually on disk,
    /// never from bookkeeping. Verification always completes before the
    /// rename. `Err` carries the job's terminal state.
    fn verify_transfer(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
        received: u64,
        advertised: Option<[u8; 32]>,
    ) -> Result<([u8; 32], DigestProvenance), JobState> {
        let key = job_key(media_key);

        // Phase 3: finalize — fsync the full file. Nothing is visible at
        // the cache path yet.
        if self.layout.sync_temp(media_key, epoch).is_err() {
            self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
            transport.close();
            return Err(JobState::Failed);
        }
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Verifying;
        }

        // Phase 4: verify on the temp file — recompute SHA-256 from the
        // bytes actually on disk, never from bookkeeping. Verification
        // always completes before the rename.
        let on_disk: [u8; 32] = match self.layout.hash_temp_prefix(media_key, epoch, received) {
            Ok((_, digest)) => digest,
            Err(_) => {
                self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
                transport.close();
                return Err(JobState::Failed);
            }
        };
        if let Some(expected) = advertised {
            if on_disk != expected {
                self.finalize_failed(media_key, epoch, OfflineError::IntegrityMismatch, true);
                transport.close();
                return Err(JobState::Failed);
            }
            return Ok((on_disk, DigestProvenance::Advertised));
        }
        let verified = if transport.double_fetch_supported() {
            self.verify_by_double_fetch(transport, media_key, epoch, received, on_disk)
        } else {
            // Neither provenance tier can supply an expected digest:
            // terminal, nothing was renamed.
            self.finalize_failed(media_key, epoch, OfflineError::IntegrityUnverifiable, true);
            Err(())
        };
        match verified {
            Ok(provenance) => Ok((on_disk, provenance)),
            Err(()) => {
                transport.close();
                Err(JobState::Failed)
            }
        }
    }

    /// Tier-2 verification: a fresh authenticated second transfer must
    /// produce the identical digest. The re-read is bounded by the first
    /// transfer's length, and the offline quota is charged once — for
    /// the committed bytes, not per fetch. `Err(())` marks the job
    /// terminal-failed with the redacted cause already recorded.
    fn verify_by_double_fetch(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
        received: u64,
        on_disk: [u8; 32],
    ) -> Result<DigestProvenance, ()> {
        transport.close();
        match Self::double_fetch_digest(transport, media_key, received) {
            Ok(second) if second == on_disk => Ok(DigestProvenance::DoubleFetch),
            Ok(_) => {
                self.finalize_failed(media_key, epoch, OfflineError::IntegrityMismatch, true);
                Err(())
            }
            Err(_) => {
                // A second transfer that cannot complete leaves no
                // verification path: terminal before publish.
                self.finalize_failed(media_key, epoch, OfflineError::IntegrityUnverifiable, true);
                Err(())
            }
        }
    }

    /// Phases 5 and 6: publish the verified temp by atomic rename, then
    /// commit the cache row. A failed rename leaves the temp in place
    /// for cleanup and the cache path untouched.
    fn commit_snapshot(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        epoch: u64,
        received: u64,
        on_disk: [u8; 32],
        provenance: DigestProvenance,
    ) -> JobState {
        let key = job_key(media_key);
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Committing;
        }
        let Ok(recorded) = self.layout.publish(media_key, epoch) else {
            self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, false);
            transport.close();
            return JobState::Failed;
        };

        // Phase 6: commit — the cache row exists only from here on.
        let snapshot = CommittedSnapshot {
            media_key: media_key.clone(),
            capability_epoch: epoch,
            byte_size: received,
            sha256_hex: hex(&on_disk),
            digest_provenance: provenance,
            cache_path: recorded,
            licence_label: OperationalLicence::SourceDeclared,
            committed_at_epoch_secs: (self.config.clock)(),
        };
        // Supersede the predecessor sibling only after this snapshot is
        // committed and integrity-checked. A predecessor at a different
        // epoch owns a different journal: retire it there. A predecessor
        // at the same epoch shares this journal, which now records the
        // fresh commit — nothing to retire.
        if let Some(predecessor) = self.index.resolve(media_key).committed_snapshot() {
            if predecessor.capability_epoch != epoch {
                let _ = self.layout.append_journal(
                    media_key,
                    predecessor.capability_epoch,
                    &JournalRecord::Retired {
                        reason: RetirementReason::Superseded,
                    },
                );
            }
        }
        let _ = self.layout.append_journal(
            media_key,
            epoch,
            &JournalRecord::Committed {
                snapshot: snapshot.clone(),
            },
        );
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Committed;
            job.current_bytes = received;
            job.current_sha256 = Some(on_disk);
            job.failure = None;
        }
        self.index.insert(snapshot);
        transport.close();
        // Opportunistic eviction: a commit that pushes the global total
        // past the quota restores headroom immediately.
        if self.index.total_committed_bytes() > self.config.global_quota_bytes {
            let _ = self.evict_to_quota();
        }
        JobState::Committed
    }

    /// The recorded state of one job, `Failed` when no record exists.
    fn current_job_state(&self, key: &JobKey) -> JobState {
        self.jobs
            .get(key)
            .map(|job| job.state)
            .unwrap_or(JobState::Failed)
    }

    /// Request cancellation of one job. The job observes the request at
    /// the next segment boundary inside [`OfflineEngine::drive`].
    /// Returns whether a non-terminal job exists for the key.
    pub fn request_cancel(&mut self, media_key: &MediaKey) -> bool {
        let key = job_key(media_key);
        match self.jobs.get(&key) {
            Some(job) if !job.state.is_terminal() => {
                self.cancel_requested.insert(key);
                true
            }
            _ => false,
        }
    }

    /// Cancel one job immediately — for when no drive is in flight. The
    /// cancellation leaves the same atomicity footprint as a quota
    /// failure: temp unlinked, no cache row created, nothing renamed.
    pub fn cancel(&mut self, media_key: &MediaKey) -> bool {
        if !self.request_cancel(media_key) {
            return false;
        }
        let key = job_key(media_key);
        self.cancel_requested.remove(&key);
        let epoch = self.jobs.get(&key).map(|job| job.capability_epoch);
        if let Some(epoch) = epoch {
            self.finalize_cancelled(media_key, epoch);
        }
        true
    }

    /// Source replacement: cancel every non-terminal job of the source
    /// whose capability epoch is older than the live epoch. Returns the
    /// number of cancelled jobs. Committed rows are untouched — they play
    /// without a live authority and retire only at reconciliation.
    pub fn supersede_source(&mut self, source_id: SourceId, live_epoch: u64) -> usize {
        let stale: Vec<JobKey> = self
            .jobs
            .iter()
            .filter(|(key, job)| {
                key.0 == source_id && !job.state.is_terminal() && job.capability_epoch < live_epoch
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &stale {
            let media_key = MediaKey::new(key.0, key.1.clone());
            let epoch = self.jobs.get(key).map(|job| job.capability_epoch);
            if let Some(epoch) = epoch {
                self.finalize_cancelled(&media_key, epoch);
            }
        }
        stale.len()
    }

    /// Re-check the operational licence of a source at reconciliation.
    /// A revoked or denied licence retires the committed rows (the files
    /// are preserved — they are the user's); re-declaring the licence is
    /// non-destructive and never un-retires a row.
    pub fn reconcile_licence(&mut self, source_id: SourceId, licence: OperationalLicence) -> usize {
        if licence == OperationalLicence::SourceDeclared {
            return 0;
        }
        let mut retired = 0;
        for snapshot in self.index.source_snapshots(source_id) {
            if self.index.is_revoked(&snapshot.media_key) {
                continue;
            }
            let _ = self.layout.append_journal(
                &snapshot.media_key,
                snapshot.capability_epoch,
                &JournalRecord::Retired {
                    reason: RetirementReason::LicenceRevoked,
                },
            );
            self.index.revoke(&snapshot.media_key);
            retired += 1;
        }
        retired
    }

    /// Execute the eviction plan against the global quota: newest-first
    /// within the oldest sources. Every victim is one content-aware step
    /// — unlink the file, then retire the row — so a failed unlink aborts
    /// the walk and leaves the row intact (no half-evicted state).
    /// Returns the number of evicted rows.
    pub fn evict_to_quota(&mut self) -> Result<usize, OfflineError> {
        let candidates = self.index.eviction_candidates();
        let victims = quota::plan_eviction(
            candidates,
            self.index.total_committed_bytes(),
            self.config.global_quota_bytes,
        );
        let mut evicted = 0;
        for media_key in victims {
            let OfflineCatalogueEntry::Cached(snapshot) = self.index.resolve(&media_key) else {
                continue;
            };
            self.layout.unlink_recorded(&snapshot.cache_path)?;
            let _ = self.layout.append_journal(
                &media_key,
                snapshot.capability_epoch,
                &JournalRecord::Retired {
                    reason: RetirementReason::Evicted,
                },
            );
            self.index.remove(&media_key);
            evicted += 1;
        }
        Ok(evicted)
    }

    /// Re-derive durable state from the journals on restart. Job state is
    /// never memory-only: committed rows rebuild the catalogue index,
    /// non-terminal journals resume (`Receiving`) or restart cleanly
    /// (`Queued`), and artifacts no recorded mapping owns are unlinked.
    fn recover(&mut self) {
        for track in self.layout.scan() {
            let parsed = parse_track_journals(&track);
            if parsed.is_empty() {
                // No surviving journal: every temp in this directory is
                // an orphan, and a final file here has no recorded
                // mapping.
                self.remove_unowned_artifacts(&track);
                continue;
            }
            self.rebuild_committed_rows(&parsed);
            let (media_key, kept_slot) = self.restore_live_job(&parsed);
            self.clean_track_orphans(&track, &media_key, &kept_slot, &parsed);
        }
    }

    /// Rebuild the committed-row index for one track directory: the
    /// highest epoch wins per media key (one journal per slot, so
    /// epochs never collide within a key). Evicted and superseded rows
    /// drop; licence-revoked rows stay (the file is preserved, the row
    /// is not playable).
    fn rebuild_committed_rows(&mut self, parsed: &[RecoveredJob]) {
        let mut committed_sorted = parsed
            .iter()
            .filter_map(|entry| {
                entry
                    .committed
                    .as_ref()
                    .map(|snapshot| (entry.retired, snapshot))
            })
            .collect::<Vec<_>>();
        committed_sorted.sort_by_key(|(_, snapshot)| snapshot.capability_epoch);
        for (retired, snapshot) in committed_sorted {
            match retired {
                Some(RetirementReason::Evicted | RetirementReason::Superseded) => {}
                Some(RetirementReason::LicenceRevoked) => {
                    self.index.insert(snapshot.clone());
                    self.index.revoke(&snapshot.media_key);
                }
                None => self.index.insert(snapshot.clone()),
            }
        }
    }

    /// Re-derive the live job for one track directory: the
    /// highest-epoch journal wins; older epochs are history. Trusted
    /// progress plus a validator plus existing temp bytes resumes as
    /// `Receiving`; anything else restarts cleanly as `Queued` with a
    /// fresh journal. Returns the job's media key and its kept temp
    /// slot.
    fn restore_live_job(&mut self, parsed: &[RecoveredJob]) -> (MediaKey, PathBuf) {
        let latest = parsed
            .iter()
            .max_by_key(|entry| entry.record.capability_epoch)
            .expect("non-empty parsed");
        let media_key = latest.record.media_key.clone();
        let epoch = latest.record.capability_epoch;
        let mut record = latest.record.clone();
        if !record.state.is_terminal() {
            if self.live_job_resumable(&media_key, epoch, latest) {
                record.state = JobState::Receiving;
                record.current_bytes = latest.journaled_offset;
            } else {
                let _ = self.layout.reset_journal(&media_key, epoch, None);
                self.layout.remove_temp(&media_key, epoch);
                record.state = JobState::Queued;
                record.current_bytes = 0;
                record.resume_validator = None;
            }
        }
        let kept_slot = self.layout.temp_path(&media_key, epoch);
        self.jobs.insert(job_key(&media_key), record);
        (media_key, kept_slot)
    }

    /// Whether the recovered job's journaled progress can be trusted to
    /// resume: journaled segments, a captured validator, existing temp
    /// bytes, and a re-verified last segment on disk.
    fn live_job_resumable(&self, media_key: &MediaKey, epoch: u64, latest: &RecoveredJob) -> bool {
        let temp_len = self.layout.temp_len(media_key, epoch);
        let resumable = latest.journaled_offset > 0
            && latest.validator.is_some()
            && latest.has_segments
            && temp_len.is_some();
        if resumable {
            return self
                .verify_resume_basis(media_key, epoch, latest.journaled_offset)
                .unwrap_or(false);
        }
        false
    }

    /// Remove orphan temps (any temp outside the kept job's slot, and
    /// the kept slot itself when the kept job is terminal — a failed
    /// publish's leftover) and the orphan final file (a cache path no
    /// surviving committed row maps to).
    fn clean_track_orphans(
        &self,
        track: &TrackArtifacts,
        media_key: &MediaKey,
        kept_slot: &Path,
        parsed: &[RecoveredJob],
    ) {
        let kept_is_terminal = self
            .jobs
            .get(&job_key(media_key))
            .is_some_and(|job| job.state.is_terminal());
        for temp in &track.temp_paths {
            if temp.as_path() != kept_slot || kept_is_terminal {
                let _ = std::fs::remove_file(temp);
            }
        }
        let row_survives = parsed.iter().any(|entry| {
            entry.committed.is_some()
                && !matches!(
                    entry.retired,
                    Some(RetirementReason::Evicted | RetirementReason::Superseded)
                )
        });
        if track.final_present && !row_survives {
            let _ = std::fs::remove_file(self.layout.root().join(track.recorded_cache_path()));
        }
    }

    /// Remove every artifact of a track directory with no surviving
    /// journal: temps, and the final file (no recorded mapping owns it).
    fn remove_unowned_artifacts(&self, track: &TrackArtifacts) {
        for temp in &track.temp_paths {
            let _ = std::fs::remove_file(temp);
        }
        if track.final_present {
            let _ = std::fs::remove_file(self.layout.root().join(track.recorded_cache_path()));
        }
    }

    /// Re-verify the last journaled segment against the bytes on disk and
    /// truncate the torn tail beyond the journaled offset. `Ok(true)` =
    /// resume-ready; `Ok(false)` = untrusted, restart from zero; `Err` =
    /// storage refused, fail the job.
    fn verify_resume_basis(
        &self,
        media_key: &MediaKey,
        epoch: u64,
        journaled: u64,
    ) -> Result<bool, OfflineError> {
        let journal_path = self.layout.journal_path(media_key, epoch);
        let last_segment = match storage::load_journal(&journal_path) {
            LoadedJournalFile::Valid(journal) => journal.segments.last().cloned(),
            LoadedJournalFile::Unusable => None,
        };
        let Some(last) = last_segment else {
            return Ok(false);
        };
        let on_disk =
            self.layout
                .hash_temp_range(media_key, epoch, last.offset, last.offset + last.len);
        match on_disk {
            Ok(digest) if hex(&digest) == last.sha256_hex => {}
            Ok(_) => return Ok(false),
            Err(_) => return Ok(false),
        }
        self.layout.truncate_temp(media_key, epoch, journaled)?;
        Ok(true)
    }

    /// Record the entity validator captured from the first successful
    /// response and rewrite the journal head to carry it. No segments
    /// exist yet on a fresh capture.
    fn capture_validator(
        &mut self,
        media_key: &MediaKey,
        epoch: u64,
        validator: Option<EntityValidator>,
    ) {
        let key = job_key(media_key);
        if let Some(job) = self.jobs.get_mut(&key) {
            job.resume_validator = validator.clone();
        }
        if let Some(validator) = validator {
            // The validator is captured at the first successful response;
            // the journal head is rewritten to carry it. No segments exist
            // yet on a fresh capture.
            let _ = self.layout.reset_journal(media_key, epoch, Some(validator));
        }
    }

    /// Discard all journaled progress for one job and prepare a clean
    /// restart from zero under the same `(media_key, capability_epoch)`
    /// slot: fresh journal, recreated temp, cleared in-memory progress.
    fn restart_from_zero(&mut self, media_key: &MediaKey, epoch: u64) {
        let _ = self.layout.reset_journal(media_key, epoch, None);
        if self.layout.reserve_temp(media_key, epoch).is_err() {
            // The temp cannot be recreated; the next receive fails
            // StorageUnavailable at its append.
        }
        let key = job_key(media_key);
        if let Some(job) = self.jobs.get_mut(&key) {
            job.current_bytes = 0;
            job.current_sha256 = None;
        }
        self.network_pauses.remove(&key);
    }

    /// Record the terminal `Failed` state on the job and its journal.
    /// `unlink_temp` is set everywhere a half-promoted temp must leave
    /// no state; it is cleared only for a failed publish, where the
    /// temp is the restart-recovery cleanup's to claim.
    fn finalize_failed(
        &mut self,
        media_key: &MediaKey,
        epoch: u64,
        error: OfflineError,
        unlink_temp: bool,
    ) {
        let key = job_key(media_key);
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Failed;
            job.failure = Some(error);
        }
        if unlink_temp {
            self.layout.remove_temp(media_key, epoch);
        }
        let _ = self.layout.append_journal(
            media_key,
            epoch,
            &JournalRecord::Terminal {
                state: storage::TerminalState::Failed,
                failure: Some(error),
            },
        );
        self.network_pauses.remove(&key);
    }

    /// Record the terminal `Cancelled` state on the job and its
    /// journal, unlink the temp, and clear any pending cancellation
    /// request.
    fn finalize_cancelled(&mut self, media_key: &MediaKey, epoch: u64) {
        let key = job_key(media_key);
        if let Some(job) = self.jobs.get_mut(&key) {
            job.state = JobState::Cancelled;
            job.failure = None;
        }
        self.layout.remove_temp(media_key, epoch);
        let _ = self.layout.append_journal(
            media_key,
            epoch,
            &JournalRecord::Terminal {
                state: storage::TerminalState::Cancelled,
                failure: None,
            },
        );
        self.network_pauses.remove(&key);
        self.cancel_requested.remove(&key);
    }

    /// Map a transport error to the job outcome. `Network` pauses the job
    /// inside the resume budget (staying resumable from the journal);
    /// everything else is terminal.
    fn on_transport_error(
        &mut self,
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        error: OfflineError,
    ) -> JobState {
        transport.close();
        let key = job_key(media_key);
        let epoch = self.jobs.get(&key).map(|job| job.capability_epoch);
        let Some(epoch) = epoch else {
            return JobState::Failed;
        };
        match error {
            OfflineError::Network => {
                let pauses = self.network_pauses.entry(key).or_insert(0);
                *pauses += 1;
                if *pauses > self.config.resume_budget {
                    self.finalize_failed(media_key, epoch, OfflineError::Network, true);
                    return JobState::Failed;
                }
                // Stay resumable: the journal holds the trusted offset.
                JobState::Receiving
            }
            OfflineError::LeaseRevoked => {
                // A lifecycle-driven revocation cancels the job.
                self.finalize_cancelled(media_key, epoch);
                JobState::Cancelled
            }
            OfflineError::AuthExpired => {
                self.finalize_failed(media_key, epoch, OfflineError::AuthExpired, true);
                JobState::Failed
            }
            OfflineError::StorageUnavailable => {
                self.finalize_failed(media_key, epoch, OfflineError::StorageUnavailable, true);
                JobState::Failed
            }
            other => {
                self.finalize_failed(media_key, epoch, other, true);
                JobState::Failed
            }
        }
    }

    /// Global committed plus other jobs' in-flight bytes (the driving
    /// job's own receive is added at the check site).
    fn global_used_excluding(&self, key: &JobKey) -> u64 {
        let inflight: u64 = self
            .jobs
            .iter()
            .filter(|(job_key, job)| job_key != &key && !job.state.is_terminal())
            .map(|(_, job)| job.current_bytes)
            .sum();
        self.index.total_committed_bytes() + inflight
    }

    /// Run the bounded double-fetch: a fresh full transfer, read up to
    /// `expected_len` bytes; any overage or shortfall is a disagreement.
    fn double_fetch_digest(
        transport: &mut dyn OfflineTransport,
        media_key: &MediaKey,
        expected_len: u64,
    ) -> Result<[u8; 32], OfflineError> {
        let head = transport.open(TransferRequest {
            media_key,
            resume_from: None,
            if_range: None,
        })?;
        let _ = head;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = transport.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            received += read as u64;
            if received > expected_len {
                return Err(OfflineError::IntegrityMismatch);
            }
            hasher.update(&buffer[..read]);
        }
        if received < expected_len {
            return Err(OfflineError::IntegrityMismatch);
        }
        Ok(hasher.finalize().into())
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Load every journal of one track directory into recovery facts.
/// Journals with no parseable head are removed as orphans; valid
/// journals become one [`RecoveredJob`] each.
fn parse_track_journals(track: &TrackArtifacts) -> Vec<RecoveredJob> {
    let mut parsed: Vec<RecoveredJob> = Vec::new();
    for journal_path in &track.journal_paths {
        match storage::load_journal(journal_path) {
            LoadedJournalFile::Unusable => {
                // No parseable head: the journal and its temp are
                // orphans.
                let _ = std::fs::remove_file(journal_path);
            }
            LoadedJournalFile::Valid(journal) => {
                let mut record =
                    JobRecord::new(journal.media_key.clone(), journal.capability_epoch);
                record.resume_validator = journal.validator.clone();
                let committed = journal.committed.clone();
                let retired = journal.retired;
                match (&committed, &journal.terminal) {
                    (Some(_), _) => {
                        record.state = JobState::Committed;
                        record.current_bytes = committed
                            .as_ref()
                            .map(|snapshot| snapshot.byte_size)
                            .unwrap_or(0);
                    }
                    (None, Some((storage::TerminalState::Failed, failure))) => {
                        record.state = JobState::Failed;
                        record.failure = *failure;
                    }
                    (None, Some((storage::TerminalState::Cancelled, _))) => {
                        record.state = JobState::Cancelled;
                    }
                    (None, None) => record.state = JobState::Queued,
                }
                parsed.push(RecoveredJob {
                    record,
                    committed,
                    retired,
                    validator: journal.validator.clone(),
                    journaled_offset: journal.journaled_offset(),
                    has_segments: !journal.segments.is_empty(),
                });
            }
        }
    }
    parsed
}

trait CommittedSnapshotExt {
    fn committed_snapshot(&self) -> Option<CommittedSnapshot>;
}

impl CommittedSnapshotExt for OfflineCatalogueEntry {
    fn committed_snapshot(&self) -> Option<CommittedSnapshot> {
        match self {
            Self::LiveOnly => None,
            Self::Cached(snapshot) | Self::Revoked(snapshot) => Some(snapshot.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn media_key(track: &str) -> MediaKey {
        MediaKey::new(SourceId::local(), TrackId::remote(track).expect("track id"))
    }

    fn source(seed: u64) -> SourceId {
        SourceId::from_uuid(uuid::Uuid::from_u64_pair(0x0aa1_0000_0000_0000, seed))
    }

    fn key_on(source_id: SourceId, track: &str) -> MediaKey {
        MediaKey::new(source_id, TrackId::remote(track).expect("track id"))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn declared() -> Result<Option<OfflineSnapshot>, OfflineError> {
        Ok(Some(OfflineSnapshot::new(u64::MAX)))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn undeclared() -> Result<Option<OfflineSnapshot>, OfflineError> {
        Ok(None)
    }

    fn refused() -> Result<Option<OfflineSnapshot>, OfflineError> {
        Err(OfflineError::UnsupportedSource)
    }

    fn test_config(quota: u64) -> EngineConfig {
        EngineConfig {
            global_quota_bytes: quota,
            segment_bytes: 16,
            resume_budget: 2,
            clock: counting_clock_next,
        }
    }

    fn sha(body: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(body);
        hasher.finalize().into()
    }

    fn body(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| seed.wrapping_add(index as u8))
            .collect()
    }

    /// Scripted transport: each `open()` consumes the next step; reads
    /// stream the step's body and can fail at a fixed cursor offset.
    struct Step {
        head: TransferHead,
        content: Result<Vec<u8>, OfflineError>,
        fail_at_cursor: Option<(usize, OfflineError)>,
    }

    struct FakeTransport {
        steps: VecDeque<Step>,
        open_error: Option<OfflineError>,
        double_fetch: bool,
        open_log: Vec<(Option<u64>, Option<EntityValidator>)>,
        close_count: usize,
        read_count: usize,
        current: Option<Step>,
        cursor: usize,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                steps: VecDeque::new(),
                open_error: None,
                double_fetch: true,
                open_log: Vec::new(),
                close_count: 0,
                read_count: 0,
                current: None,
                cursor: 0,
            }
        }

        fn push(mut self, head: TransferHead, content: Vec<u8>) -> Self {
            self.steps.push_back(Step {
                head,
                content: Ok(content),
                fail_at_cursor: None,
            });
            self
        }

        fn push_failing(
            mut self,
            head: TransferHead,
            body: Vec<u8>,
            at_cursor: usize,
            error: OfflineError,
        ) -> Self {
            self.steps.push_back(Step {
                head,
                content: Ok(body),
                fail_at_cursor: Some((at_cursor, error)),
            });
            self
        }

        fn with_open_error(mut self, error: OfflineError) -> Self {
            self.open_error = Some(error);
            self
        }

        fn without_double_fetch(mut self) -> Self {
            self.double_fetch = false;
            self
        }

        fn fresh_head(
            validator: Option<EntityValidator>,
            advertised: Option<[u8; 32]>,
            total: Option<u64>,
        ) -> TransferHead {
            TransferHead {
                outcome: TransferOutcome::Fresh,
                total_length: total,
                validator,
                advertised_sha256: advertised,
                lease: Some(LeaseId::from_raw(42)),
            }
        }

        fn partial_head(
            validator: Option<EntityValidator>,
            advertised: Option<[u8; 32]>,
        ) -> TransferHead {
            TransferHead {
                outcome: TransferOutcome::Partial,
                total_length: None,
                validator,
                advertised_sha256: advertised,
                lease: Some(LeaseId::from_raw(43)),
            }
        }
    }

    impl OfflineTransport for FakeTransport {
        fn open(&mut self, request: TransferRequest<'_>) -> Result<TransferHead, OfflineError> {
            self.open_log
                .push((request.resume_from, request.if_range.cloned()));
            if let Some(error) = self.open_error {
                self.open_error = None;
                return Err(error);
            }
            let step = self.steps.pop_front().ok_or(OfflineError::Network)?;
            let head = step.head.clone();
            self.cursor = 0;
            self.current = Some(step);
            Ok(head)
        }

        fn read(&mut self, buffer: &mut [u8]) -> Result<usize, OfflineError> {
            self.read_count += 1;
            let Some(step) = self.current.as_mut() else {
                return Err(OfflineError::Network);
            };
            if let Some((at, error)) = step.fail_at_cursor {
                if self.cursor >= at {
                    return Err(error);
                }
            }
            match &mut step.content {
                Err(_) => Err(OfflineError::StorageUnavailable),
                Ok(body) => {
                    let want = buffer.len().min(body.len() - self.cursor);
                    buffer[..want].copy_from_slice(&body[self.cursor..self.cursor + want]);
                    self.cursor += want;
                    Ok(want)
                }
            }
        }

        fn double_fetch_supported(&self) -> bool {
            self.double_fetch
        }

        fn close(&mut self) {
            self.close_count += 1;
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn etag(value: &str) -> Option<EntityValidator> {
        Some(EntityValidator::ETag(value.to_string()))
    }

    #[test]
    fn admission_refuses_undeclared_refused_and_denied_before_any_network() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");

        let transport = FakeTransport::new();
        assert_eq!(
            engine.admit(&key, 1, &undeclared(), OperationalLicence::SourceDeclared),
            Err(OfflineError::UnsupportedSource)
        );
        assert_eq!(
            engine.admit(&key, 1, &refused(), OperationalLicence::SourceDeclared),
            Err(OfflineError::UnsupportedSource)
        );
        assert_eq!(
            engine.admit(&key, 1, &declared(), OperationalLicence::Denied),
            Err(OfflineError::LicenceDenied)
        );
        assert_eq!(
            engine.admit(&key, 1, &declared(), OperationalLicence::Revoked),
            Err(OfflineError::LicenceDenied)
        );
        // No network work happened for any refusal.
        assert_eq!(transport.open_log.len(), 0);
        assert!(engine.job(&key).is_none());
    }

    #[test]
    fn duplicate_admission_waits_and_a_committed_epoch_short_circuits() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let payload = body(40, 1);
        let digest = sha(&payload);

        assert_eq!(
            engine
                .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
                .expect("admit"),
            Admission::Started
        );
        assert_eq!(
            engine
                .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
                .expect("admit"),
            Admission::InFlight
        );

        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(digest), None),
            payload,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );

        // Same epoch: identical content by definition.
        assert_eq!(
            engine
                .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
                .expect("admit"),
            Admission::Current
        );
        // A newer epoch is a genuine refresh.
        assert_eq!(
            engine
                .admit(&key, 2, &declared(), OperationalLicence::SourceDeclared)
                .expect("admit"),
            Admission::Started
        );
    }

    #[test]
    fn fresh_download_with_advertised_digest_commits_atomically() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let payload = body(40, 7);
        let digest = sha(&payload);

        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(digest), Some(40)),
            payload.clone(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );

        let job = engine.job(&key).expect("job");
        assert_eq!(job.state, JobState::Committed);
        assert_eq!(job.current_bytes, 40);
        assert_eq!(job.current_sha256, Some(digest));
        assert_eq!(job.resume_validator, etag("\"v1\""));

        let OfflineCatalogueEntry::Cached(snapshot) = engine.catalogue(&key) else {
            panic!("committed row must be cached");
        };
        assert_eq!(snapshot.byte_size, 40);
        assert_eq!(snapshot.sha256_hex, hex(&digest));
        assert_eq!(snapshot.digest_provenance, DigestProvenance::Advertised);
        assert_eq!(snapshot.capability_epoch, 1);
        // The published file is the only on-disk copy; the temp is gone.
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("published bytes"),
            payload
        );
        assert!(!engine.layout().temp_path(&key, 1).exists());
        assert_eq!(engine.total_committed_bytes(), 40);

        // Restart recovery rebuilds the row from the committed journal.
        let reopened = OfflineEngine::open(dir.path(), test_config(4096)).expect("reopen");
        assert!(matches!(
            reopened.catalogue(&key),
            OfflineCatalogueEntry::Cached(_)
        ));
        assert_eq!(reopened.total_committed_bytes(), 40);
    }

    #[test]
    fn double_fetch_verification_commits_and_disagreement_fails_integrity() {
        let dir = tempfile::tempdir().expect("root");
        let payload = body(40, 3);
        let other = body(40, 9);

        // Equal second transfer: DoubleFetch provenance, quota charged once.
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new()
            .push(
                FakeTransport::fresh_head(etag("\"v1\""), None, None),
                payload.clone(),
            )
            .push(FakeTransport::fresh_head(None, None, None), payload.clone());
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        let OfflineCatalogueEntry::Cached(snapshot) = engine.catalogue(&key) else {
            panic!("committed row must be cached");
        };
        assert_eq!(snapshot.digest_provenance, DigestProvenance::DoubleFetch);
        assert_eq!(snapshot.byte_size, 40, "quota charged once, not per fetch");
        assert_eq!(transport.open_log.len(), 2, "first transfer + double fetch");
        drop(engine);

        // Disagreeing second transfer: IntegrityMismatch, nothing renamed.
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine 2");
        let key2 = media_key("track-2");
        engine
            .admit(&key2, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new()
            .push(
                FakeTransport::fresh_head(etag("\"v1\""), None, None),
                payload,
            )
            .push(FakeTransport::fresh_head(None, None, None), other);
        assert_eq!(engine.drive(&mut transport, &key2), Some(JobState::Failed));
        let job = engine.job(&key2).expect("job");
        assert_eq!(job.failure, Some(OfflineError::IntegrityMismatch));
        assert_eq!(engine.catalogue(&key2), OfflineCatalogueEntry::LiveOnly);
        assert!(!engine.layout().temp_path(&key2, 1).exists());
        assert!(
            !engine.layout().final_path(&key2).exists(),
            "nothing was renamed"
        );
    }

    #[test]
    fn no_verification_path_fails_unverifiable_before_any_rename() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().without_double_fetch().push(
            FakeTransport::fresh_head(etag("\"v1\""), None, None),
            body(40, 1),
        );
        assert_eq!(engine.drive(&mut transport, &key), Some(JobState::Failed));
        assert_eq!(
            engine.job(&key).and_then(|job| job.failure),
            Some(OfflineError::IntegrityUnverifiable)
        );
        assert_eq!(engine.catalogue(&key), OfflineCatalogueEntry::LiveOnly);
        assert!(!engine.layout().final_path(&key).exists());
        assert!(!engine.layout().temp_path(&key, 1).exists());
    }

    #[test]
    fn an_oversized_byte_hint_fails_before_receiving_a_byte() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(
                etag("\"v1\""),
                None,
                Some(crate::architecture::MAX_OFFLINE_BYTE_HINT + 1),
            ),
            body(40, 1),
        );
        assert_eq!(engine.drive(&mut transport, &key), Some(JobState::Failed));
        assert_eq!(
            engine.job(&key).and_then(|job| job.failure),
            Some(OfflineError::StorageUnavailable)
        );
        assert_eq!(
            transport.read_count, 0,
            "no byte received after hostile hint"
        );
        assert!(!engine.layout().temp_path(&key, 1).exists());
    }

    #[test]
    fn an_advertised_media_length_above_the_legacy_four_kib_hint_commits() {
        let dir = tempfile::tempdir().expect("root");
        let config = EngineConfig {
            segment_bytes: 4096,
            ..test_config(1024 * 1024)
        };
        let mut engine = OfflineEngine::open(dir.path(), config).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        // A real media payload: its advertised length sits far above the
        // legacy 4 KiB hint ceiling and far below the per-file media
        // limit, so the fresh transfer must commit.
        let payload = body(64 * 1024, 3);
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(
                etag("\"v1\""),
                Some(sha(&payload)),
                Some(payload.len() as u64),
            ),
            payload,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        assert_eq!(
            engine.job(&key).map(|job| job.current_bytes),
            Some(64 * 1024)
        );
        assert!(engine.layout().final_path(&key).exists());
    }

    #[test]
    fn network_pause_then_restart_recovery_resumes_from_the_journaled_offset() {
        let dir = tempfile::tempdir().expect("root");
        let payload = body(40, 5);
        let digest = sha(&payload);

        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(etag("\"v1\""), Some(digest), None),
            payload.clone(),
            16,
            OfflineError::Network,
        );
        // One committed segment (16 bytes), then a transient pause.
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Receiving)
        );
        assert_eq!(engine.job(&key).expect("job").current_bytes, 16);
        drop(engine);

        // "Process restart": a fresh engine re-derives Receiving state.
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("reopen");
        let job = engine.job(&key).expect("job");
        assert_eq!(job.state, JobState::Receiving);
        assert_eq!(job.current_bytes, 16);

        // The resume request carries the journaled offset and If-Range.
        let mut transport = FakeTransport::new().push(
            FakeTransport::partial_head(etag("\"v1\""), Some(digest)),
            payload[16..].to_vec(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        assert_eq!(
            transport.open_log[0].0,
            Some(16),
            "Range starts at the journaled offset"
        );
        assert_eq!(
            transport.open_log[0].1,
            etag("\"v1\""),
            "If-Range carries the captured validator"
        );
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("published"),
            payload,
            "resumed file matches the full entity"
        );
    }

    #[test]
    fn a_resume_answered_fresh_discards_partial_bytes_and_restarts_from_zero() {
        let dir = tempfile::tempdir().expect("root");
        let payload = body(40, 6);
        let digest = sha(&payload);

        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(etag("\"v1\""), Some(digest), None),
            payload.clone(),
            16,
            OfflineError::Network,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Receiving)
        );

        // Entity changed (200): the job restarts from zero on this transfer.
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v2\""), Some(digest), None),
            payload.clone(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("published"),
            payload,
            "the file is the entity, not partial plus full"
        );
        assert_eq!(engine.job(&key).expect("job").current_bytes, 40);
        assert_eq!(
            engine.job(&key).expect("job").resume_validator,
            etag("\"v2\""),
            "the new validator is captured on the fresh answer"
        );
    }

    #[test]
    fn journal_progress_without_a_validator_restarts_fully() {
        let dir = tempfile::tempdir().expect("root");
        let payload = body(40, 8);
        let digest = sha(&payload);

        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(None, Some(digest), None),
            payload.clone(),
            16,
            OfflineError::Network,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Receiving)
        );

        // Same session: no validator means a full restart, no Range.
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(None, Some(digest), None),
            payload.clone(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        assert_eq!(transport.open_log[0].0, None, "restart never resumes");
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("published"),
            payload
        );

        // Across a restart: recovery also refuses to resume validator-less
        // progress and re-derives Queued.
        let dir2 = tempfile::tempdir().expect("root 2");
        let mut engine = OfflineEngine::open(dir2.path(), test_config(4096)).expect("engine");
        let key2 = media_key("track-1");
        engine
            .admit(&key2, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(None, Some(sha(&payload)), None),
            payload.clone(),
            16,
            OfflineError::Network,
        );
        assert_eq!(
            engine.drive(&mut transport, &key2),
            Some(JobState::Receiving)
        );
        drop(engine);
        let engine = OfflineEngine::open(dir2.path(), test_config(4096)).expect("reopen");
        let job = engine.job(&key2).expect("job");
        assert_eq!(
            job.state,
            JobState::Queued,
            "validator-less progress is discarded"
        );
        assert_eq!(job.current_bytes, 0);
    }

    #[test]
    fn cancellation_is_decisive_and_leaves_the_quota_failure_footprint() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let payload = body(64, 2);
        let digest = sha(&payload);
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");

        assert!(engine.request_cancel(&key));
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(digest), None),
            payload,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Cancelled)
        );
        assert_eq!(engine.catalogue(&key), OfflineCatalogueEntry::LiveOnly);
        assert!(!engine.layout().temp_path(&key, 1).exists());
        assert!(!engine.layout().final_path(&key).exists());
        assert!(
            !engine.request_cancel(&key),
            "terminal job takes no cancellation"
        );
        assert!(!engine.cancel(&key));

        // Direct cancellation (no drive in flight) behaves identically.
        let key2 = media_key("track-2");
        engine
            .admit(&key2, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        assert!(engine.cancel(&key2));
        assert_eq!(engine.job(&key2).expect("job").state, JobState::Cancelled);
        assert!(!engine.layout().temp_path(&key2, 1).exists());
    }

    #[test]
    fn source_replacement_supersedes_stale_epoch_jobs() {
        let dir = tempfile::tempdir().expect("root");
        let owner = source(11);
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = key_on(owner, "track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(etag("\"v1\""), None, None),
            body(64, 1),
            16,
            OfflineError::Network,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Receiving)
        );

        // The source replaced itself at epoch 2: the stale job cancels.
        assert_eq!(engine.supersede_source(owner, 2), 1);
        assert_eq!(engine.job(&key).expect("job").state, JobState::Cancelled);
        assert!(!engine.layout().temp_path(&key, 1).exists());
        assert_eq!(
            engine.supersede_source(owner, 2),
            0,
            "nothing left to cancel"
        );

        // The replacement admits cleanly at the new epoch.
        assert_eq!(
            engine
                .admit(&key, 2, &declared(), OperationalLicence::SourceDeclared)
                .expect("admit"),
            Admission::Started
        );
    }

    #[test]
    fn quota_overrun_mid_receive_fails_terminally_and_cleans_up() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(32)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), None, None),
            body(100, 1),
        );
        assert_eq!(engine.drive(&mut transport, &key), Some(JobState::Failed));
        assert_eq!(
            engine.job(&key).and_then(|job| job.failure),
            Some(OfflineError::QuotaExceeded)
        );
        assert_eq!(engine.catalogue(&key), OfflineCatalogueEntry::LiveOnly);
        assert!(!engine.layout().temp_path(&key, 1).exists());
        assert_eq!(engine.total_committed_bytes(), 0);
    }

    #[test]
    fn eviction_walks_oldest_sources_first_and_newest_snapshots_first_within_one() {
        let dir = tempfile::tempdir().expect("root");
        let a = source(21);
        let b = source(22);
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");

        // A1(t=1, 100 bytes), A2(t=2, 200 bytes), B1(t=3, 400 bytes).
        for (owner, track, len, seed) in [
            (a, "track-1", 100usize, 1u8),
            (a, "track-2", 200, 2),
            (b, "track-1", 400, 3),
        ] {
            let key = key_on(owner, track);
            engine
                .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
                .expect("admit");
            let payload = body(len, seed);
            let mut transport = FakeTransport::new().push(
                FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&payload)), None),
                payload.clone(),
            );
            assert_eq!(
                engine.drive(&mut transport, &key),
                Some(JobState::Committed)
            );
            assert!(engine.layout().final_path(&key).exists());
        }
        assert_eq!(engine.total_committed_bytes(), 700);

        // Quota 650: 50 bytes over. Oldest source is A; newest-first
        // within A evicts A2 (200 bytes) — enough, and B is untouched.
        let mut dir_reopened = OfflineEngine::open(dir.path(), test_config(650)).expect("reopen");
        assert_eq!(dir_reopened.evict_to_quota(), Ok(1));
        let a2 = key_on(a, "track-2");
        assert_eq!(dir_reopened.catalogue(&a2), OfflineCatalogueEntry::LiveOnly);
        assert!(
            !dir_reopened.layout().final_path(&a2).exists(),
            "evicted file is unlinked"
        );
        assert_ne!(
            dir_reopened.catalogue(&key_on(a, "track-1")),
            OfflineCatalogueEntry::LiveOnly
        );
        assert_ne!(
            dir_reopened.catalogue(&key_on(b, "track-1")),
            OfflineCatalogueEntry::LiveOnly
        );
        assert_eq!(dir_reopened.total_committed_bytes(), 500);
        drop(dir_reopened);

        // Quota 250: A1 goes next (its source is still oldest), then B1.
        let mut engine = OfflineEngine::open(dir.path(), test_config(250)).expect("reopen 2");
        assert_eq!(engine.evict_to_quota(), Ok(2));
        assert_eq!(
            engine.catalogue(&key_on(a, "track-1")),
            OfflineCatalogueEntry::LiveOnly
        );
        assert_eq!(
            engine.catalogue(&key_on(b, "track-1")),
            OfflineCatalogueEntry::LiveOnly
        );
        assert_eq!(engine.total_committed_bytes(), 0);
        assert!(!engine.layout().final_path(&key_on(a, "track-1")).exists());
        assert!(!engine.layout().final_path(&key_on(b, "track-1")).exists());
    }

    #[test]
    fn refresh_supersedes_the_predecessor_only_after_the_new_commit_verifies() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let v1 = body(40, 1);
        let v2 = body(40, 2);

        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit v1");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&v1)), None),
            v1.clone(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );

        // A failed refresh must not retire the predecessor.
        engine
            .admit(&key, 2, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit v2");
        let wrong = sha(&v2);
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v2\""), Some(wrong), None),
            body(40, 3),
        );
        assert_eq!(engine.drive(&mut transport, &key), Some(JobState::Failed));
        let OfflineCatalogueEntry::Cached(predecessor) = engine.catalogue(&key) else {
            panic!("predecessor survives a failed refresh");
        };
        assert_eq!(predecessor.capability_epoch, 1);
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("v1 bytes"),
            v1,
            "the committed file is preserved through the failed refresh"
        );

        // The good refresh commits and supersedes the sibling.
        engine
            .admit(&key, 3, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit v3");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v3\""), Some(sha(&v2)), None),
            v2.clone(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        let OfflineCatalogueEntry::Cached(fresh) = engine.catalogue(&key) else {
            panic!("refresh committed");
        };
        assert_eq!(fresh.capability_epoch, 3);
        assert_eq!(
            engine.source_snapshots(SourceId::local()).len(),
            1,
            "no sibling accumulation"
        );
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("v2 bytes"),
            v2
        );

        // Recovery agrees: only the new snapshot survives.
        let reopened = OfflineEngine::open(dir.path(), test_config(4096)).expect("reopen");
        assert_eq!(reopened.source_snapshots(SourceId::local()).len(), 1);
    }

    #[test]
    fn licence_revocation_retires_rows_preserves_files_and_never_unretires() {
        let dir = tempfile::tempdir().expect("root");
        let owner = source(31);
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = key_on(owner, "track-1");
        let payload = body(40, 4);
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&payload)), None),
            payload,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        let final_path = engine.layout().final_path(&key);

        assert_eq!(
            engine.reconcile_licence(owner, OperationalLicence::Revoked),
            1
        );
        assert!(matches!(
            engine.catalogue(&key),
            OfflineCatalogueEntry::Revoked(_)
        ));
        assert!(final_path.exists(), "revocation preserves the file");

        // Re-declaring is non-destructive: committed rows stay retired.
        assert_eq!(
            engine.reconcile_licence(owner, OperationalLicence::SourceDeclared),
            0
        );
        assert!(matches!(
            engine.catalogue(&key),
            OfflineCatalogueEntry::Revoked(_)
        ));
        assert!(final_path.exists());

        // Eviction skips revoked rows: their bytes occupy the quota but no
        // playable victim exists.
        assert_eq!(engine.evict_to_quota(), Ok(0));

        // New downloads under a revoked licence are refused at admission.
        assert_eq!(
            engine.admit(&key, 2, &declared(), OperationalLicence::Revoked),
            Err(OfflineError::LicenceDenied)
        );

        // Recovery keeps the revoked state.
        let reopened = OfflineEngine::open(dir.path(), test_config(4096)).expect("reopen");
        assert!(matches!(
            reopened.catalogue(&key),
            OfflineCatalogueEntry::Revoked(_)
        ));
        assert!(final_path.exists());
    }

    #[test]
    fn recovery_rebuilds_rows_and_cleans_orphan_artifacts() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let payload = body(40, 6);
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&payload)), None),
            payload,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        drop(engine);

        // Orphan artifacts: a temp and a final file no journal owns.
        let orphan_dir = dir.path().join("a".repeat(32)).join("b".repeat(32));
        std::fs::create_dir_all(&orphan_dir).expect("orphan dir");
        std::fs::write(orphan_dir.join("media.bin.part-zzz"), b"torn").expect("orphan temp");
        std::fs::write(orphan_dir.join("media.bin"), b"unmapped").expect("orphan final");

        let reopened = OfflineEngine::open(dir.path(), test_config(4096)).expect("reopen");
        assert!(matches!(
            reopened.catalogue(&key),
            OfflineCatalogueEntry::Cached(_)
        ));
        assert_eq!(reopened.total_committed_bytes(), 40);
        assert!(
            !orphan_dir.join("media.bin.part-zzz").exists(),
            "orphan temp cleaned"
        );
        assert!(
            !orphan_dir.join("media.bin").exists(),
            "unmapped final cleaned"
        );
        // The committed row's own file is untouched.
        assert!(reopened.layout().final_path(&key).exists());
    }

    #[test]
    fn a_torn_temp_tail_beyond_the_journal_is_discarded_on_resume() {
        let dir = tempfile::tempdir().expect("root");
        let payload = body(40, 9);
        let digest = sha(&payload);

        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(etag("\"v1\""), Some(digest), None),
            payload.clone(),
            16,
            OfflineError::Network,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Receiving)
        );

        // Simulate an interrupted write: junk bytes beyond the journaled
        // offset.
        let temp = engine.layout().temp_path(&key, 1);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&temp)
            .expect("temp");
        std::io::Write::write_all(&mut file, b"junk!").expect("torn tail");
        drop(file);

        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("reopen");
        assert_eq!(engine.job(&key).expect("job").current_bytes, 16);
        assert_eq!(
            engine.layout().temp_len(&key, 1),
            Some(16),
            "torn tail truncated"
        );

        let mut transport = FakeTransport::new().push(
            FakeTransport::partial_head(etag("\"v1\""), Some(digest)),
            payload[16..].to_vec(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );
        assert_eq!(
            std::fs::read(engine.layout().final_path(&key)).expect("published"),
            payload,
            "the torn tail never reaches the committed bytes"
        );
    }

    #[test]
    fn auth_expiry_fails_and_lease_revocation_cancels() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(etag("\"v1\""), None, None),
            body(40, 1),
            0,
            OfflineError::AuthExpired,
        );
        assert_eq!(engine.drive(&mut transport, &key), Some(JobState::Failed));
        assert_eq!(
            engine.job(&key).and_then(|job| job.failure),
            Some(OfflineError::AuthExpired),
            "auth expiry is a terminal failure"
        );
        assert!(!engine.layout().temp_path(&key, 1).exists());

        let key2 = media_key("track-2");
        engine
            .admit(&key2, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push_failing(
            FakeTransport::fresh_head(etag("\"v1\""), None, None),
            body(40, 1),
            0,
            OfflineError::LeaseRevoked,
        );
        assert_eq!(
            engine.drive(&mut transport, &key2),
            Some(JobState::Cancelled)
        );
        assert_eq!(
            engine.job(&key2).and_then(|job| job.failure),
            None,
            "cancellation carries no failure cause"
        );
        assert!(!engine.layout().temp_path(&key2, 1).exists());
    }

    #[test]
    fn a_failed_job_can_retry_at_the_same_epoch_and_drive_is_none_for_unknown_keys() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let payload = body(40, 4);

        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&body(40, 5))), None),
            payload.clone(),
        );
        assert_eq!(engine.drive(&mut transport, &key), Some(JobState::Failed));

        // Retry at the same epoch: a fresh job in the same slot.
        assert_eq!(
            engine
                .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
                .expect("retry admit"),
            Admission::Started
        );
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&payload)), None),
            payload.clone(),
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );

        assert_eq!(
            engine.drive(&mut transport, &media_key("never-admitted")),
            None
        );
    }

    #[test]
    fn the_engine_never_persists_a_url_or_credential_shaped_string() {
        let dir = tempfile::tempdir().expect("root");
        let mut engine = OfflineEngine::open(dir.path(), test_config(4096)).expect("engine");
        let key = media_key("track-1");
        let payload = body(40, 1);
        engine
            .admit(&key, 1, &declared(), OperationalLicence::SourceDeclared)
            .expect("admit");
        let mut transport = FakeTransport::new().push(
            FakeTransport::fresh_head(etag("\"v1\""), Some(sha(&payload)), None),
            payload,
        );
        assert_eq!(
            engine.drive(&mut transport, &key),
            Some(JobState::Committed)
        );

        let OfflineCatalogueEntry::Cached(snapshot) = engine.catalogue(&key) else {
            panic!("cached");
        };
        for forbidden in ["http://", "https://", "token=", "password=", "Bearer "] {
            assert!(
                !snapshot.cache_path.contains(forbidden),
                "cache path leaked {forbidden:?}"
            );
        }
        // Every path component of the recorded mapping is fixed-charset.
        for component in snapshot.cache_path.split('/') {
            assert!(
                component == "media.bin"
                    || (component.len() == 32
                        && component
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())),
                "component {component:?} is not a derived key or the constant name"
            );
        }
    }
}
