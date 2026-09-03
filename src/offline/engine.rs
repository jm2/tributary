//! The headless download/cache engine supervisor.
//!
//! One engine instance is owned by exactly one supervisor (the source
//! registry's offline worker or a future headless application owner); it is
//! never driven from a GTK thread. Jobs advance through the typed
//! [`JobState`] machine from [`crate::architecture::offline`]:
//!
//! `Queued → Connecting → Receiving → Verifying → Committing → Committed`,
//! with terminal `Failed`/`Cancelled` outcomes that never become playable
//! rows. Network failures consume a bounded retry budget on the same job;
//! every other failure is terminal. A resumed job revalidates the entity
//! with the captured validator; a `200`/`412` answer discards the partial
//! bytes and restarts the same job from zero.
//!
//! All failures are the redacted, structured [`OfflineError`] taxonomy: no
//! URL, header value, body excerpt, credential, or raw status ever reaches
//! the UI projection ([`OfflineBoard`]).

use std::collections::HashMap;

use crate::architecture::identity::{MediaKey, SourceId};
use crate::architecture::offline::{
    JobRecord, JobState, LeaseId, OfflineCatalogueEntry, OfflineError, OperationalLicence,
};

use super::catalog::OfflineCatalog;
use super::quota::{next_eviction_victim, EvictionCandidate, QuotaLedger};
use super::storage::{CacheStore, PublishCheck, TempReservation};

/// Bounded retry budget for transient [`OfflineError::Network`] failures on
/// one job. Exhausting the budget is terminal.
pub const MAX_NETWORK_RETRIES: u32 = 3;

/// Bounded number of full restarts allowed when a server answers a validated
/// resume with `200`/`412` (entity changed). Exhausting the budget is
/// terminal: the source cannot serve a stable entity.
pub const MAX_ENTITY_RESTARTS: u32 = 2;

/// Receive segment size for range reads and journaled progress.
pub const SEGMENT_BYTES: u64 = 4096;

/// Upper bound on internal phase transitions within one `drive()` pass.
/// Generous for any real transfer (64 steps ≈ 256 KiB at [`SEGMENT_BYTES`]);
/// a supervisor driving pacing-sensitive retries simply calls again.
const MAX_DRIVE_STEPS: usize = 64;

/// Outcome of one receive-phase step.
enum DriveStep {
    /// State advanced; continue the pass.
    Advance,
    /// Transient network pause: stop the pass (budget already consumed).
    Stop,
    /// The entity changed under a validated resume: restart from zero.
    Changed,
    /// An error to route through the retry/terminal budget.
    Fail(OfflineError),
}

/// Why [`OfflineEngine::admit`] refused a job before any network work.
///
/// These refusals are admission decisions, not job failures: no `JobRecord`
/// exists afterwards, so they deliberately do not consume the
/// [`OfflineError`] failure taxonomy that only terminal jobs carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionRefusal {
    /// A non-terminal job already owns this `(media_key, capability_epoch)`;
    /// the newer request must wait for the predecessor's terminal state.
    AlreadyInFlight,
    /// The source has not declared a position, or declared
    /// `OperationalLicence::Denied`/`Revoked` at admission.
    LicenceDenied,
    /// The backend explicitly refuses offline capability.
    UnsupportedSource,
    /// The source supplied a byte hint the cache layer cannot trust
    /// (beyond the contract's `MAX_OFFLINE_BYTE_HINT` ceiling).
    ByteHintUntrusted,
}

/// The source's declared offline position, mirroring the default-deny
/// capability: `Ok(None)` (not declared) is distinct from an explicit
/// refusal and from an opted-in licence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SourceOfflinePosition {
    /// `Ok(None)`: the source has not declared; offline is unavailable.
    #[default]
    NotDeclared,
    /// `Ok(Some(snapshot))` with the source's current licence.
    Declared(OperationalLicence),
    /// `Err(OfflineError::UnsupportedSource)`: explicit refusal.
    Unsupported,
}

/// Credential-free display metadata the caller supplies at admission.
///
/// These are the structured labels the source itself published (title,
/// artist, album, source label such as `Subsonic — example.com`). No URL,
/// token, or path may appear here; the UI projection truncates for display
/// and the redaction test enforces the boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OfflineRowLabels {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub source_label: String,
}

/// Open (or re-open) a transfer for one media key.
pub struct TransferOpen {
    /// Entity validator captured from the first successful response.
    /// `None` disables resumption: the job restarts fully instead.
    pub resume_validator: Option<crate::architecture::offline::EntityValidator>,
    /// A digest advertised by a provenance tier; `None` requires the
    /// engine's double-fetch verification.
    pub advertised_digest: Option<[u8; 32]>,
    /// Live total size used only for progress display; never persisted as
    /// a trusted hint.
    pub total_bytes: Option<u64>,
}

/// The answer to one validated range read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeOutcome {
    /// `206 Partial Content` with exactly the requested range.
    Partial(Vec<u8>),
    /// `200`/`412` on an `If-Range` request: the entity changed or cannot
    /// be range-validated; the job restarts from zero under the same id.
    EntityChanged,
}

/// The network seam between the engine and a source adapter.
///
/// The real HTTP adapter (exact-origin proxy lane, redirect matrix) is a
/// follow-up slice; tests implement this trait to drive the full state
/// machine deterministically. Errors are already-redacted
/// [`OfflineError`] values — the adapter layer performs any mapping.
pub trait TransferBackend {
    /// Open the transfer: capture the validator, advertised digest, and
    /// live total for this media key.
    ///
    /// Implementations must validate the caller-supplied capability before
    /// any byte moves; the engine treats an `Err` here as terminal.
    fn open(&mut self, key: &MediaKey) -> Result<TransferOpen, OfflineError>;

    /// Read one range, revalidating with `If-Range` when a validator exists.
    fn read_range(
        &mut self,
        key: &MediaKey,
        validator: Option<&crate::architecture::offline::EntityValidator>,
        start: u64,
        len: u64,
    ) -> Result<RangeOutcome, OfflineError>;

    /// Independent second transfer for double-fetch verification.
    fn double_fetch(&mut self, key: &MediaKey) -> Result<[u8; 32], OfflineError>;
}

/// UI-visible view of one committed snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedRowView {
    pub byte_size: u64,
    pub committed_at_epoch_secs: u64,
    pub licence_label: OperationalLicence,
    /// `false` for licence-revoked rows: preserved on disk, not playable.
    pub playable: bool,
}

/// One credential-free row of the UI projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineRowSnapshot {
    pub media_key: MediaKey,
    pub labels: OfflineRowLabels,
    /// Live job state when a job row exists (`Committed` for settled cache
    /// rows, `Failed`/`Cancelled` for terminal rows kept for retry/delete).
    pub state: JobState,
    pub failure: Option<OfflineError>,
    pub current_bytes: u64,
    pub total_bytes: Option<u64>,
    pub cached: Option<CachedRowView>,
}

/// The aggregate UI projection the GTK storage panel renders.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OfflineBoard {
    pub rows: Vec<OfflineRowSnapshot>,
    pub committed_bytes: u64,
    pub quota_bytes: u64,
}

struct ActiveJob {
    record: JobRecord,
    labels: OfflineRowLabels,
    reservation: Option<TempReservation>,
    network_retries: u32,
    entity_restarts: u32,
    total_bytes: Option<u64>,
    advertised_digest: Option<[u8; 32]>,
    /// Set when a transient failure paused a receiving job: the next
    /// receive step must apply resume discipline (torn-tail trim or
    /// validator-less full restart) before reading the remainder.
    resume_pending: bool,
}

impl ActiveJob {
    fn terminal(&self) -> bool {
        self.record.state.is_terminal()
    }
}

/// The single-owner download/cache supervisor.
pub struct OfflineEngine<B: TransferBackend> {
    backend: B,
    store: CacheStore,
    ledger: QuotaLedger,
    catalog: OfflineCatalog,
    jobs: HashMap<MediaKey, ActiveJob>,
    sources: HashMap<SourceId, SourceOfflinePosition>,
    epochs: HashMap<SourceId, u64>,
    nonce: u64,
    now_epoch_secs: u64,
}

impl<B: TransferBackend> OfflineEngine<B> {
    /// Build an engine over `backend`, `store`, and a global byte quota.
    #[must_use]
    pub fn new(backend: B, store: CacheStore, quota_bytes: u64) -> Self {
        Self {
            backend,
            store,
            ledger: QuotaLedger::new(quota_bytes),
            catalog: OfflineCatalog::default(),
            jobs: HashMap::new(),
            sources: HashMap::new(),
            epochs: HashMap::new(),
            nonce: 0,
            now_epoch_secs: 0,
        }
    }

    /// Declare (or re-declare) one source's offline position.
    pub fn set_source_position(&mut self, source: SourceId, position: SourceOfflinePosition) {
        self.sources.insert(source, position);
    }

    /// Bump a source's capability generation (replacement/reconnection).
    /// Stale in-flight jobs are cancelled; committed rows survive.
    pub fn bump_epoch(&mut self, source: &SourceId) {
        let epoch = self.epochs.entry(*source).or_insert(0);
        *epoch += 1;
        let stale: Vec<MediaKey> = self
            .jobs
            .iter()
            .filter(|(key, job)| {
                !job.terminal() && key.source_id == *source && job.record.capability_epoch < *epoch
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.cancel(&key);
        }
    }

    /// The current capability generation for one source.
    #[must_use]
    pub fn epoch_of(&self, source: &SourceId) -> u64 {
        self.epochs.get(source).copied().unwrap_or(0)
    }

    /// Advance the engine's wall clock (committed-at stamps, eviction age).
    pub fn advance_clock(&mut self, epoch_secs: u64) {
        self.now_epoch_secs = self.now_epoch_secs.max(epoch_secs);
    }

    /// Admit one download job for `key`.
    ///
    /// Default-deny: the source must have declared
    /// [`SourceOfflinePosition::Declared(OperationalLicence::SourceDeclared)`].
    /// Refusal happens before any network work and leaves no job row.
    pub fn admit(
        &mut self,
        key: MediaKey,
        labels: OfflineRowLabels,
        requested_bytes_hint: Option<u64>,
    ) -> Result<(), AdmissionRefusal> {
        if let Some(hint) = requested_bytes_hint {
            if crate::architecture::offline::validate_byte_hint(hint).is_err() {
                return Err(AdmissionRefusal::ByteHintUntrusted);
            }
        }
        match self
            .sources
            .get(&key.source_id)
            .copied()
            .unwrap_or_default()
        {
            SourceOfflinePosition::NotDeclared => return Err(AdmissionRefusal::LicenceDenied),
            SourceOfflinePosition::Unsupported => return Err(AdmissionRefusal::UnsupportedSource),
            SourceOfflinePosition::Declared(OperationalLicence::SourceDeclared) => {}
            SourceOfflinePosition::Declared(
                OperationalLicence::Denied | OperationalLicence::Revoked,
            ) => {
                return Err(AdmissionRefusal::LicenceDenied);
            }
        }
        if let Some(job) = self.jobs.get(&key) {
            if !job.terminal() {
                return Err(AdmissionRefusal::AlreadyInFlight);
            }
        }
        let mut record = JobRecord::new(key.clone(), self.epoch_of(&key.source_id));
        record.requested_bytes = requested_bytes_hint;
        self.jobs.insert(
            key,
            ActiveJob {
                record,
                labels,
                reservation: None,
                network_retries: 0,
                entity_restarts: 0,
                resume_pending: false,
                total_bytes: None,
                advertised_digest: None,
            },
        );
        Ok(())
    }

    /// Drive one job as far as it can go: to a terminal state, or to a
    /// pending retry after a transient network failure.
    ///
    /// Returns the job's state after this pass (`None` when no job exists).
    /// Each phase runs as its own step method so the job borrow is always
    /// ended before a multi-field `&mut self` helper runs.
    pub fn drive(&mut self, key: &MediaKey) -> Option<JobState> {
        for _ in 0..MAX_DRIVE_STEPS {
            let state = self.jobs.get(key)?.record.state;
            if state.is_terminal() {
                return Some(state);
            }
            match state {
                JobState::Queued | JobState::Connecting => {
                    let connect = self.step_connect(key);
                    if let DriveStep::Fail(err) = connect {
                        if self.retry_or_fail(key, err) {
                            return Some(state);
                        }
                    }
                }
                JobState::Receiving => match self.step_receive(key) {
                    DriveStep::Advance => {}
                    DriveStep::Stop => return Some(state),
                    DriveStep::Changed => self.restart_from_zero(key),
                    DriveStep::Fail(err) => {
                        if self.retry_or_fail(key, err) {
                            return Some(state);
                        }
                    }
                },
                JobState::Verifying | JobState::Committing => self.step_verify(key),
                JobState::Committed | JobState::Failed | JobState::Cancelled => return Some(state),
            }
        }
        // Bounded step guard; practically unreachable for any real transfer.
        self.jobs.get(key).map(|job| job.record.state)
    }

    /// Connect phase: open the transfer, capture the validator, digest tier,
    /// and live total, and mint the temp reservation.
    fn step_connect(&mut self, key: &MediaKey) -> DriveStep {
        let opened = {
            let Some(job) = self.jobs.get_mut(key) else {
                return DriveStep::Advance;
            };
            job.record.state = JobState::Connecting;
            self.backend.open(key)
        };
        match opened {
            Ok(opened) => {
                let Some(job) = self.jobs.get_mut(key) else {
                    return DriveStep::Advance;
                };
                job.record.resume_validator = opened.resume_validator;
                job.advertised_digest = opened.advertised_digest;
                job.total_bytes = opened.total_bytes;
                job.record.state = JobState::Receiving;
                // Mint the reservation inline: fields (`jobs`, `store`,
                // `nonce`) are borrowed disjointly here.
                self.nonce += 1;
                match self.store.reserve_temp(key, self.nonce) {
                    Ok(reservation) => {
                        job.reservation = Some(reservation);
                        job.record.last_lease = Some(LeaseId::from_raw(self.nonce));
                    }
                    Err(_) => {
                        job.reservation = None;
                        job.record.state = JobState::Failed;
                        job.record.failure = Some(OfflineError::StorageUnavailable);
                    }
                }
                DriveStep::Advance
            }
            Err(err) => DriveStep::Fail(err),
        }
    }

    /// Receive phase: apply the resume discipline, read one segment, journal
    /// it durably, and advance on completion.
    fn step_receive(&mut self, key: &MediaKey) -> DriveStep {
        // Gather inputs under a short job borrow.
        let (want, journaled, validator, resume_possible, resuming) = {
            let Some(job) = self.jobs.get_mut(key) else {
                return DriveStep::Advance;
            };
            if job.record.state != JobState::Receiving {
                return DriveStep::Advance;
            }
            let want = match job.total_bytes {
                Some(total) => total
                    .saturating_sub(job.record.current_bytes)
                    .min(SEGMENT_BYTES),
                None => SEGMENT_BYTES,
            };
            if want == 0 {
                job.record.state = JobState::Verifying;
                return DriveStep::Advance;
            }
            if job.reservation.is_none() {
                return DriveStep::Fail(OfflineError::StorageUnavailable);
            }
            let resuming = job.resume_pending;
            (
                want,
                job.record.current_bytes,
                job.record.resume_validator.clone(),
                job.record.resume_validator.is_some(),
                resuming,
            )
        };
        let journaled_len = if resume_possible { journaled } else { 0 };
        // Resume discipline: a job without a captured validator restarts
        // fully; a job with one truncates any torn tail back to the
        // journaled offset. The journaled offset is the only trusted resume
        // state — the raw on-disk length never is. Applied exactly once per
        // pause, then the flag clears so continuous segments flow freely.
        let trimmed = {
            let Some(job) = self.jobs.get_mut(key) else {
                return DriveStep::Advance;
            };
            if !resuming {
                false
            } else {
                job.resume_pending = false;
                if !resume_possible && journaled > 0 {
                    job.record.current_bytes = 0;
                }
                let Some(reservation) = job.reservation.as_ref() else {
                    return DriveStep::Fail(OfflineError::StorageUnavailable);
                };
                let disk_len = super::storage::size_on_disk(reservation.temp_path());
                if disk_len > journaled_len {
                    self.store
                        .truncate_temp(reservation, journaled_len)
                        .is_err()
                } else {
                    disk_len < journaled_len
                }
            }
        };
        if trimmed {
            return DriveStep::Fail(OfflineError::StorageUnavailable);
        }
        // The read offset is the job's journaled position after the
        // discipline above (a validator-less resume has just reset it to 0).
        let start = self.jobs.get(key).map_or(0, |job| job.record.current_bytes);
        let outcome = self
            .backend
            .read_range(key, validator.as_ref(), start, want);
        match outcome {
            Ok(RangeOutcome::Partial(bytes)) => {
                let write = {
                    let Some(job) = self.jobs.get_mut(key) else {
                        return DriveStep::Advance;
                    };
                    let Some(reservation) = job.reservation.as_ref() else {
                        return DriveStep::Fail(OfflineError::StorageUnavailable);
                    };
                    self.store.write_segment(reservation, start, &bytes)
                };
                match write {
                    Ok(_) => {
                        let Some(job) = self.jobs.get_mut(key) else {
                            return DriveStep::Advance;
                        };
                        job.record.current_bytes = start + bytes.len() as u64;
                        let done = match job.total_bytes {
                            Some(total) => job.record.current_bytes >= total,
                            None => (bytes.len() as u64) < want,
                        };
                        if done {
                            job.record.state = JobState::Verifying;
                        }
                        DriveStep::Advance
                    }
                    Err(err) => DriveStep::Fail(err),
                }
            }
            Ok(RangeOutcome::EntityChanged) => DriveStep::Changed,
            Err(err) => DriveStep::Fail(err),
        }
    }

    /// Verify phase: resolve the digest check, enforce quota, publish.
    fn step_verify(&mut self, key: &MediaKey) {
        let check = {
            let Some(job) = self.jobs.get_mut(key) else {
                return;
            };
            job.record.state = JobState::Verifying;
            match job.advertised_digest {
                Some(advertised) => Some(PublishCheck::Advertised(advertised)),
                None => match self.backend.double_fetch(key) {
                    // The contract is explicit: a second transfer that
                    // cannot complete is terminal — the tier could not
                    // produce a comparable digest.
                    Ok(second_digest) => Some(PublishCheck::DoubleFetch(second_digest)),
                    Err(_) => None,
                },
            }
        };
        let Some(check) = check else {
            self.fail(key, OfflineError::IntegrityUnverifiable);
            return;
        };
        if let Some(job) = self.jobs.get_mut(key) {
            job.record.state = JobState::Committing;
        }
        // Records both terminal outcomes on the job itself.
        let _unused = self.enforce_quota_then_publish(
            key,
            check,
            OperationalLicence::SourceDeclared,
            self.now_epoch_secs,
        );
    }

    /// Drive every non-terminal job once.
    pub fn drive_all(&mut self) {
        let keys: Vec<MediaKey> = self
            .jobs
            .iter()
            .filter(|(_, job)| !job.terminal())
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.drive(&key);
        }
    }

    /// User- or lifecycle-driven cancellation. A cancelled job leaves the
    /// same atomicity footprint as a quota failure: temp unlinked, no cache
    /// row, no half-promoted UI state. Terminal jobs are left untouched.
    pub fn cancel(&mut self, key: &MediaKey) -> bool {
        let Some(job) = self.jobs.get_mut(key) else {
            return false;
        };
        if job.terminal() {
            return false;
        }
        if let Some(reservation) = job.reservation.take() {
            let _unused = std::fs::remove_file(reservation.temp_path());
        }
        job.record.state = JobState::Cancelled;
        job.record.last_lease = None;
        true
    }

    /// Cancel every in-flight job owned by one source (disconnect/logout).
    /// Committed rows survive: a logout revokes only the in-flight lease.
    pub fn on_source_disconnected(&mut self, source: &SourceId) {
        let in_flight: Vec<MediaKey> = self
            .jobs
            .iter()
            .filter(|(key, job)| !job.terminal() && key.source_id == *source)
            .map(|(key, _)| key.clone())
            .collect();
        for key in in_flight {
            self.cancel(&key);
        }
    }

    /// Reconcile a licence revocation: committed rows are retired (not
    /// playable) while their files are preserved; in-flight jobs are
    /// cancelled, mirroring lease revocation.
    pub fn reconcile_licence_revoked(&mut self, source: &SourceId) {
        self.set_source_position(
            *source,
            SourceOfflinePosition::Declared(OperationalLicence::Revoked),
        );
        self.on_source_disconnected(source);
        let keys: Vec<MediaKey> = self
            .catalog
            .snapshots_for_source(*source)
            .iter()
            .map(|snapshot| snapshot.media_key.clone())
            .collect();
        for key in keys {
            self.catalog.retire(&key);
        }
    }

    /// User-driven "Remove download": unlink the file, drop the row, release
    /// the quota charge. Returns `false` when nothing was cached.
    pub fn delete_cached(&mut self, key: &MediaKey) -> Result<bool, OfflineError> {
        let Some(snapshot) = self.catalog.remove(key) else {
            return Ok(false);
        };
        self.store.unlink_snapshot(&snapshot)?;
        self.ledger.release(snapshot.byte_size);
        Ok(true)
    }

    /// The offline catalogue read for one media key.
    #[must_use]
    pub fn catalogue(&self, key: &MediaKey) -> OfflineCatalogueEntry {
        self.catalog.resolve(key)
    }

    /// Render the credential-free board projection the storage panel shows.
    #[must_use]
    pub fn board(&self) -> OfflineBoard {
        let mut keys: Vec<MediaKey> = self.jobs.keys().cloned().collect();
        for snapshot in self.catalog.all_snapshots() {
            if !self.jobs.contains_key(&snapshot.media_key) {
                keys.push(snapshot.media_key);
            }
        }
        keys.sort_by(|left, right| left.track_id.as_str().cmp(right.track_id.as_str()));
        let rows = keys
            .into_iter()
            .map(|key| {
                let cached_view = match self.catalog.resolve(&key) {
                    OfflineCatalogueEntry::Cached(snapshot) => Some(CachedRowView {
                        byte_size: snapshot.byte_size,
                        committed_at_epoch_secs: snapshot.committed_at_epoch_secs,
                        licence_label: snapshot.licence_label,
                        playable: true,
                    }),
                    OfflineCatalogueEntry::Revoked(snapshot) => Some(CachedRowView {
                        byte_size: snapshot.byte_size,
                        committed_at_epoch_secs: snapshot.committed_at_epoch_secs,
                        licence_label: snapshot.licence_label,
                        playable: false,
                    }),
                    OfflineCatalogueEntry::LiveOnly => None,
                };
                match self.jobs.get(&key) {
                    Some(job) => OfflineRowSnapshot {
                        media_key: key,
                        labels: job.labels.clone(),
                        state: job.record.state,
                        failure: job.record.failure,
                        current_bytes: job.record.current_bytes,
                        total_bytes: job.total_bytes,
                        cached: cached_view,
                    },
                    None => OfflineRowSnapshot {
                        media_key: key,
                        labels: OfflineRowLabels::default(),
                        state: JobState::Committed,
                        failure: None,
                        current_bytes: 0,
                        total_bytes: None,
                        cached: cached_view,
                    },
                }
            })
            .collect();
        OfflineBoard {
            rows,
            committed_bytes: self.ledger.committed_bytes(),
            quota_bytes: self.ledger.quota_bytes(),
        }
    }

    // -- internals ---------------------------------------------------------

    fn reserve_for(&mut self, key: &MediaKey) {
        self.nonce += 1;
        let nonce = self.nonce;
        if let Some(job) = self.jobs.get_mut(key) {
            match self.store.reserve_temp(key, nonce) {
                Ok(reservation) => {
                    job.reservation = Some(reservation);
                    job.record.last_lease = Some(LeaseId::from_raw(nonce));
                }
                Err(_) => {
                    job.reservation = None;
                    job.record.state = JobState::Failed;
                    job.record.failure = Some(OfflineError::StorageUnavailable);
                }
            }
        }
    }

    /// Handle one engine-step error: a transient `Network` failure consumes
    /// one unit of the resume budget and pauses the pass (the supervisor
    /// paces the next `drive()` call — this is where backoff belongs in the
    /// future HTTP adapter); every other error is terminal. Returns `true`
    /// when the job paused and the pass must stop.
    fn retry_or_fail(&mut self, key: &MediaKey, err: OfflineError) -> bool {
        if err == OfflineError::Network {
            let Some(job) = self.jobs.get_mut(key) else {
                return false;
            };
            job.network_retries += 1;
            if job.network_retries > MAX_NETWORK_RETRIES {
                job.record.state = JobState::Failed;
                job.record.failure = Some(OfflineError::Network);
                self.release_reservation(key);
                return false;
            }
            if job.record.state == JobState::Receiving {
                job.resume_pending = true;
            }
            return true;
        }
        self.fail(key, err);
        false
    }

    fn fail(&mut self, key: &MediaKey, err: OfflineError) {
        if let Some(job) = self.jobs.get_mut(key) {
            job.record.state = JobState::Failed;
            job.record.failure = Some(err);
            job.record.last_lease = None;
        }
        self.release_reservation(key);
    }

    fn restart_from_zero(&mut self, key: &MediaKey) {
        let Some(job) = self.jobs.get_mut(key) else {
            return;
        };
        job.entity_restarts += 1;
        if job.entity_restarts > MAX_ENTITY_RESTARTS {
            // The source cannot serve a stable entity: the job's restart
            // budget is exhausted.
            job.record.state = JobState::Failed;
            job.record.failure = Some(OfflineError::Network);
            self.release_reservation(key);
            return;
        }
        // Discard the partial bytes and restart the same job from zero.
        // Re-opening the transfer re-captures the validator for the new
        // entity, exactly as a fresh full fetch would.
        job.record.current_bytes = 0;
        job.record.state = JobState::Connecting;
        if let Some(reservation) = job.reservation.as_ref() {
            let _unused = self.store.truncate_temp(reservation, 0);
        }
    }

    fn release_reservation(&mut self, key: &MediaKey) {
        if let Some(job) = self.jobs.get_mut(key) {
            if let Some(reservation) = job.reservation.take() {
                let _unused = std::fs::remove_file(reservation.temp_path());
            }
        }
    }

    /// Quota enforcement happens exactly once, at the commit point: evict
    /// (transactionally — row deleted and file unlinked in one step) until
    /// the new snapshot fits, else fail the job `QuotaExceeded` terminally.
    fn enforce_quota_then_publish(
        &mut self,
        key: &MediaKey,
        check: PublishCheck,
        licence: OperationalLicence,
        committed_at: u64,
    ) -> Result<(), OfflineError> {
        // Measure the received bytes so eviction targets the real size.
        let total = {
            let Some(job) = self.jobs.get_mut(key) else {
                return Err(OfflineError::StorageUnavailable);
            };
            let Some(reservation) = job.reservation.as_ref() else {
                return Err(OfflineError::StorageUnavailable);
            };
            super::storage::size_on_disk(reservation.temp_path())
        };
        if total > self.ledger.quota_bytes() {
            // No amount of eviction can ever make this file fit: fail the
            // job without destroying any committed row.
            self.fail(key, OfflineError::QuotaExceeded);
            return Err(OfflineError::QuotaExceeded);
        }
        if !self.ledger.admits(total) {
            loop {
                let candidates: Vec<EvictionCandidate> = self
                    .catalog
                    .all_snapshots()
                    .into_iter()
                    .filter(|snapshot| &snapshot.media_key != key)
                    .map(|snapshot| EvictionCandidate {
                        key: snapshot.media_key.clone(),
                        byte_size: snapshot.byte_size,
                        committed_at_epoch_secs: snapshot.committed_at_epoch_secs,
                    })
                    .collect();
                let Some(victim) = next_eviction_victim(&candidates) else {
                    break;
                };
                let removed = self.delete_cached(&victim.key).unwrap_or(false);
                if removed && self.ledger.admits(total) {
                    break;
                }
                if !removed {
                    // The victim would not unlink; nothing else will fit.
                    break;
                }
            }
            if !self.ledger.admits(total) {
                self.fail(key, OfflineError::QuotaExceeded);
                return Err(OfflineError::QuotaExceeded);
            }
        }
        let reservation = self
            .jobs
            .get_mut(key)
            .and_then(|job| job.reservation.take())
            .ok_or(OfflineError::StorageUnavailable)?;
        let epoch = self
            .jobs
            .get(key)
            .map_or(0, |job| job.record.capability_epoch);
        let snapshot =
            self.store
                .verify_and_publish(reservation, check, key, epoch, licence, committed_at);
        match snapshot {
            Ok(snapshot) => {
                let byte_size = snapshot.byte_size;
                if let Some(predecessor) = self.catalog.publish(snapshot) {
                    // Refresh sibling bound: the superseded snapshot is
                    // unlinked and its charge released in the same step.
                    let _unused = self.store.unlink_snapshot(&predecessor);
                    self.ledger.release(predecessor.byte_size);
                }
                self.ledger.commit(byte_size);
                if let Some(job) = self.jobs.get_mut(key) {
                    job.record.state = JobState::Committed;
                    job.record.last_lease = None;
                }
                Ok(())
            }
            Err(err) => {
                self.fail(key, err);
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::identity::TrackId;
    use crate::architecture::offline::EntityValidator;
    use sha2::{Digest, Sha256};

    const QUOTA: u64 = 10 * 1024;

    fn source(n: u128) -> SourceId {
        SourceId::from_uuid(uuid::Uuid::from_u128(0xA000_0000 + n))
    }

    fn key(src: SourceId, track: &str) -> MediaKey {
        MediaKey::new(src, TrackId::new(track).unwrap())
    }

    fn payload(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u8).wrapping_add(seed))
            .collect()
    }

    fn sha(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn labels(title: &str) -> OfflineRowLabels {
        OfflineRowLabels {
            title: title.to_string(),
            artist: "Artist".to_string(),
            album: Some("Album".to_string()),
            source_label: "Subsonic — example.com".to_string(),
        }
    }

    /// A programmable source adapter that drives the full engine state
    /// machine deterministically. All failure modes from the contract's
    /// failure table are reachable by configuration.
    #[allow(
        clippy::struct_excessive_bools,
        reason = "a test fake exposing every independently-triggerable contract failure mode"
    )]
    struct FakeServer {
        payload: Vec<u8>,
        etag: String,
        advertised_digest: Option<[u8; 32]>,
        fail_at_op: Option<u32>,
        auth_expire_at_op: Option<u32>,
        mutate_at_op: Option<(u32, Vec<u8>)>,
        no_validator: bool,
        reads: Vec<(u64, u64)>,
        double_fetch_digest: Option<[u8; 32]>,
        double_fetch_fails: bool,
        offline_after_open: bool,
        ops: u32,
        expired: bool,
    }

    impl FakeServer {
        fn serving(payload: Vec<u8>) -> Self {
            Self {
                advertised_digest: Some(sha(&payload)),
                payload,
                etag: "\"v1\"".to_string(),
                fail_at_op: None,
                auth_expire_at_op: None,
                mutate_at_op: None,
                no_validator: false,
                reads: Vec::new(),
                double_fetch_digest: None,
                double_fetch_fails: false,
                offline_after_open: false,
                ops: 0,
                expired: false,
            }
        }

        fn without_advertised_digest(mut self) -> Self {
            self.advertised_digest = None;
            self
        }

        /// Transient network failure on read operation `op` (1-based), so an
        /// earlier segment lands first and the job pauses mid-transfer.
        fn failing_read_at(mut self, op: u32) -> Self {
            self.fail_at_op = Some(op);
            self
        }

        fn auth_expires_at(mut self, op: u32) -> Self {
            self.auth_expire_at_op = Some(op);
            self
        }

        fn swaps_content_at(mut self, op: u32, replacement: Vec<u8>) -> Self {
            self.mutate_at_op = Some((op, replacement));
            self
        }

        fn without_validator(mut self) -> Self {
            self.no_validator = true;
            self
        }

        fn with_wrong_advertised_digest(mut self) -> Self {
            let mut wrong = sha(&self.payload);
            wrong[0] = wrong[0].wrapping_add(1);
            self.advertised_digest = Some(wrong);
            self
        }

        fn with_lying_double_fetch(mut self) -> Self {
            let mut lie = sha(&self.payload);
            lie[31] = lie[31].wrapping_add(1);
            self.double_fetch_digest = Some(lie);
            self
        }

        fn with_failing_double_fetch(mut self) -> Self {
            self.double_fetch_fails = true;
            self
        }

        fn goes_offline_after_open(mut self) -> Self {
            self.offline_after_open = true;
            self
        }
    }

    impl TransferBackend for FakeServer {
        fn open(&mut self, _key: &MediaKey) -> Result<TransferOpen, OfflineError> {
            if self.expired {
                return Err(OfflineError::AuthExpired);
            }
            let validator = if self.no_validator {
                None
            } else {
                Some(EntityValidator::ETag(self.etag.clone()))
            };
            Ok(TransferOpen {
                resume_validator: validator,
                advertised_digest: self.advertised_digest,
                total_bytes: Some(self.payload.len() as u64),
            })
        }

        fn read_range(
            &mut self,
            _key: &MediaKey,
            validator: Option<&EntityValidator>,
            start: u64,
            len: u64,
        ) -> Result<RangeOutcome, OfflineError> {
            self.ops += 1;
            if self.offline_after_open {
                return Err(OfflineError::Network);
            }
            if self.fail_at_op == Some(self.ops) {
                return Err(OfflineError::Network);
            }
            if let Some(at) = self.auth_expire_at_op {
                if self.ops >= at {
                    self.expired = true;
                    return Err(OfflineError::AuthExpired);
                }
            }
            if let Some((at, replacement)) = &self.mutate_at_op {
                if self.ops == *at {
                    self.payload = replacement.clone();
                    self.etag = "\"v2-stale\"".to_string();
                }
            }
            if let Some(EntityValidator::ETag(tag)) = validator {
                if *tag != self.etag {
                    // 200/412 on If-Range: the entity changed.
                    return Ok(RangeOutcome::EntityChanged);
                }
            }
            self.reads.push((start, len));
            if start >= self.payload.len() as u64 {
                return Ok(RangeOutcome::Partial(Vec::new()));
            }
            let end = (start + len).min(self.payload.len() as u64);
            Ok(RangeOutcome::Partial(
                self.payload[start as usize..end as usize].to_vec(),
            ))
        }

        fn double_fetch(&mut self, _key: &MediaKey) -> Result<[u8; 32], OfflineError> {
            if self.double_fetch_fails {
                return Err(OfflineError::Network);
            }
            Ok(self
                .double_fetch_digest
                .unwrap_or_else(|| sha(&self.payload)))
        }
    }

    fn engine(server: FakeServer, quota: u64) -> (OfflineEngine<FakeServer>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let engine = OfflineEngine::new(server, CacheStore::new(dir.path()), quota);
        (engine, dir)
    }

    fn declared(engine: &mut OfflineEngine<FakeServer>, src: SourceId) {
        engine.set_source_position(
            src,
            SourceOfflinePosition::Declared(OperationalLicence::SourceDeclared),
        );
    }

    fn committed_snapshot(
        engine: &OfflineEngine<FakeServer>,
        media: &MediaKey,
    ) -> crate::architecture::offline::CommittedSnapshot {
        match engine.catalogue(media) {
            OfflineCatalogueEntry::Cached(snapshot) => snapshot,
            other => panic!("expected cached snapshot, got {other:?}"),
        }
    }

    fn drive_until_terminal(engine: &mut OfflineEngine<FakeServer>, media: &MediaKey) -> JobState {
        // Generous bound: a full transfer plus restarts and retry pauses
        // stays far under this for every fixture used here.
        for _ in 0..32 {
            if let Some(state) = engine.drive(media) {
                if state.is_terminal() {
                    return state;
                }
            }
        }
        panic!("job never reached a terminal state");
    }

    // -- admission ---------------------------------------------------------

    #[test]
    fn admission_is_default_deny() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(16, 0)), QUOTA);
        let media = key(source(1), "track-1");
        // Undeclared: refused before any network work.
        assert_eq!(
            engine.admit(media.clone(), labels("t"), None),
            Err(AdmissionRefusal::LicenceDenied)
        );
        // Explicitly unsupported: structurally distinct.
        engine.set_source_position(source(1), SourceOfflinePosition::Unsupported);
        assert_eq!(
            engine.admit(media.clone(), labels("t"), None),
            Err(AdmissionRefusal::UnsupportedSource)
        );
        // Denied and revoked licences refuse; declared admits.
        engine.set_source_position(
            source(1),
            SourceOfflinePosition::Declared(OperationalLicence::Denied),
        );
        assert_eq!(
            engine.admit(media.clone(), labels("t"), None),
            Err(AdmissionRefusal::LicenceDenied)
        );
        engine.set_source_position(
            source(1),
            SourceOfflinePosition::Declared(OperationalLicence::Revoked),
        );
        assert_eq!(
            engine.admit(media.clone(), labels("t"), None),
            Err(AdmissionRefusal::LicenceDenied)
        );
        declared(&mut engine, source(1));
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    }

    #[test]
    fn admission_refuses_in_flight_duplicates_and_untrusted_byte_hints() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(16, 0)), QUOTA);
        let media = key(source(1), "track-1");
        declared(&mut engine, source(1));
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        // One job per media key: the newer request waits.
        assert_eq!(
            engine.admit(media.clone(), labels("t"), None),
            Err(AdmissionRefusal::AlreadyInFlight)
        );
        // A hint above the contract's trusted ceiling is malformed input.
        assert_eq!(
            engine.admit(key(source(1), "track-2"), labels("t"), Some(5 * 1024)),
            Err(AdmissionRefusal::ByteHintUntrusted)
        );
        // After the predecessor reaches a terminal state, a fresh job is
        // admitted cleanly.
        engine.cancel(&media);
        assert_eq!(engine.admit(media, labels("t"), None), Ok(()));
    }

    // -- the online-to-offline transition ----------------------------------

    #[test]
    fn committed_rows_play_offline_while_live_rows_do_not() {
        let expected_bytes = payload(10 * 1024, 7);
        let (mut engine, _cache_root) = engine(FakeServer::serving(expected_bytes.clone()), QUOTA);
        let src = source(1);
        declared(&mut engine, src);
        let offline = key(src, "offline-track");
        let live = key(src, "live-track");
        engine.advance_clock(100);
        assert_eq!(
            engine.admit(offline.clone(), labels("offline"), None),
            Ok(())
        );
        assert_eq!(
            drive_until_terminal(&mut engine, &offline),
            JobState::Committed
        );
        // Offline rendering: the committed row is playable with no live
        // authority, and its bytes on disk hash to the committed digest.
        let snapshot = committed_snapshot(&engine, &offline);
        assert_eq!(snapshot.byte_size, expected_bytes.len() as u64);
        assert_eq!(snapshot.sha256_hex, hex_of(&sha(&expected_bytes)));
        let on_disk = std::fs::read(&snapshot.cache_path).unwrap();
        assert_eq!(on_disk, expected_bytes);

        // The source goes away (disconnect/logout of the live session).
        engine.on_source_disconnected(&src);
        assert!(matches!(
            engine.catalogue(&offline),
            OfflineCatalogueEntry::Cached(_)
        ));

        // A track that never committed is live-only; a download attempt
        // while the network is unreachable exhausts the resume budget and
        // fails with the redacted transient-network cause.
        engine.backend = FakeServer::serving(payload(1024, 9)).goes_offline_after_open();
        engine.advance_clock(200);
        assert_eq!(engine.admit(live.clone(), labels("live"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &live), JobState::Failed);
        let board = engine.board();
        let offline_row = board
            .rows
            .iter()
            .find(|row| row.media_key == offline)
            .unwrap();
        assert_eq!(offline_row.state, JobState::Committed);
        assert!(offline_row.cached.as_ref().unwrap().playable);
        let live_row = board.rows.iter().find(|row| row.media_key == live).unwrap();
        assert_eq!(live_row.state, JobState::Failed);
        assert_eq!(live_row.failure, Some(OfflineError::Network));
    }

    #[test]
    fn licence_revocation_retires_rows_but_preserves_files() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(2048, 3)), QUOTA);
        let src = source(2);
        declared(&mut engine, src);
        let media = key(src, "licensed-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(
            drive_until_terminal(&mut engine, &media),
            JobState::Committed
        );
        let path = committed_snapshot(&engine, &media).cache_path.clone();

        engine.reconcile_licence_revoked(&src);
        match engine.catalogue(&media) {
            OfflineCatalogueEntry::Revoked(snapshot) => {
                assert_eq!(snapshot.byte_size, 2048);
            }
            other => panic!("expected revoked, got {other:?}"),
        }
        // The file is the user's: retirement preserves it on disk.
        assert!(std::path::Path::new(&path).exists());
        // Board marks it not playable.
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == media)
            .unwrap();
        assert!(!row.cached.unwrap().playable);
    }

    // -- stale servers ------------------------------------------------------

    #[test]
    fn a_stale_server_serving_changed_content_restarts_from_zero_and_commits() {
        let original = payload(10 * 1024, 1);
        let replacement = payload(10 * 1024, 2);
        // The server swaps its entity mid-transfer: every read from op 2 on
        // answers with the new content under a new ETag.
        let server = FakeServer::serving(original)
            .without_advertised_digest()
            .swaps_content_at(2, replacement.clone());
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(3);
        declared(&mut engine, src);
        let media = key(src, "stale-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(
            drive_until_terminal(&mut engine, &media),
            JobState::Committed
        );

        let server_state = engine.board();
        let row = server_state
            .rows
            .into_iter()
            .find(|row| row.media_key == media)
            .unwrap();
        assert_eq!(row.state, JobState::Committed);
        // The committed bytes are the CURRENT entity, not the stale prefix.
        let snapshot = committed_snapshot(&engine, &media);
        let on_disk = std::fs::read(&snapshot.cache_path).unwrap();
        assert_eq!(on_disk, replacement);
        // The journal restarted: the entity was re-read from zero after the
        // swap.
        let zero_reads = engine
            .backend
            .reads
            .iter()
            .filter(|(start, _)| *start == 0)
            .count();
        assert!(
            zero_reads >= 2,
            "expected the entity to be re-read from zero after the swap"
        );
    }

    #[test]
    fn a_lying_advertised_digest_fails_integrity_and_never_publishes() {
        let server = FakeServer::serving(payload(4096, 5)).with_wrong_advertised_digest();
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(4);
        declared(&mut engine, src);
        let media = key(src, "corrupt-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
        assert_eq!(
            engine.catalogue(&media),
            OfflineCatalogueEntry::LiveOnly,
            "a failed row never becomes playable"
        );
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == media)
            .unwrap();
        assert_eq!(row.failure, Some(OfflineError::IntegrityMismatch));
        // No half-promoted state: the track directory holds no file at all.
        let track_dir = engine.store.track_dir(&media);
        let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn a_double_fetch_disagreement_is_terminal_integrity_mismatch() {
        let server = FakeServer::serving(payload(4096, 6))
            .without_advertised_digest()
            .with_lying_double_fetch();
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(5);
        declared(&mut engine, src);
        let media = key(src, "double-fetch-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
        assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
    }

    #[test]
    fn an_incomplete_double_fetch_is_integrity_unverifiable() {
        let server = FakeServer::serving(payload(4096, 6))
            .without_advertised_digest()
            .with_failing_double_fetch();
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(6);
        declared(&mut engine, src);
        let media = key(src, "unverifiable-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == media)
            .unwrap();
        assert_eq!(row.failure, Some(OfflineError::IntegrityUnverifiable));
    }

    // -- partial files -------------------------------------------------------

    #[test]
    fn a_paused_job_resumes_from_the_journaled_offset_and_heals_torn_tails() {
        let payload = payload(10 * 1024, 8); // 3 segments at 4 KiB
        let server = FakeServer::serving(payload.clone()).failing_read_at(2);
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(7);
        declared(&mut engine, src);
        let media = key(src, "resume-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        // First pass: segment 1 lands, the second read hits the transient
        // failure and the pass pauses with 4096 journaled bytes.
        assert_eq!(engine.drive(&media), Some(JobState::Receiving));
        assert_eq!(engine.jobs[&media].record.current_bytes, 4096);

        // Simulate an interrupted write: a torn tail exists on disk beyond
        // the journaled offset.
        let reservation = engine.jobs[&media].reservation.as_ref().unwrap();
        let torn = reservation.temp_path().to_path_buf();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&torn)
                .unwrap();
            file.write_all(b"TORN-TAIL").unwrap();
        }

        // The resume pass truncates the torn tail, revalidates with If-Range,
        // and continues from exactly the journaled offset.
        assert_eq!(
            drive_until_terminal(&mut engine, &media),
            JobState::Committed
        );
        assert!(engine.backend.reads.iter().any(|(start, _)| *start == 4096));
        let snapshot = committed_snapshot(&engine, &media);
        let on_disk = std::fs::read(&snapshot.cache_path).unwrap();
        assert_eq!(on_disk, payload);
        assert_eq!(snapshot.byte_size, payload.len() as u64);
    }

    #[test]
    fn a_job_without_a_validator_resumes_by_full_restart_only() {
        let payload = payload(10 * 1024, 9);
        let server = FakeServer::serving(payload.clone())
            .without_validator()
            .failing_read_at(2);
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(8);
        declared(&mut engine, src);
        let media = key(src, "restart-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(engine.drive(&media), Some(JobState::Receiving));
        assert_eq!(engine.jobs[&media].record.current_bytes, 4096);
        assert_eq!(
            drive_until_terminal(&mut engine, &media),
            JobState::Committed
        );
        // Every read after the pause starts from zero again.
        let zero_reads = engine
            .backend
            .reads
            .iter()
            .filter(|(start, _)| *start == 0)
            .count();
        assert!(
            zero_reads >= 2,
            "resume without a validator must restart from zero"
        );
        let snapshot = committed_snapshot(&engine, &media);
        assert_eq!(std::fs::read(&snapshot.cache_path).unwrap(), payload);
    }

    #[test]
    fn auth_expiry_mid_download_is_terminal_and_leaves_no_row() {
        let server = FakeServer::serving(payload(10 * 1024, 10)).auth_expires_at(2);
        let (mut engine, _dir) = engine(server, QUOTA);
        let src = source(9);
        declared(&mut engine, src);
        let media = key(src, "auth-track");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == media)
            .unwrap();
        assert_eq!(row.failure, Some(OfflineError::AuthExpired));
        assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
        let track_dir = engine.store.track_dir(&media);
        let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
        assert!(leftovers.is_empty());
    }

    // -- quota pressure -------------------------------------------------------

    #[test]
    fn quota_pressure_evicts_oldest_source_first_then_newest_within_source() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(1024, 1)), QUOTA);
        let a = source(10);
        let b = source(11);
        declared(&mut engine, a);
        declared(&mut engine, b);

        // Committed: b-1 at t=50 (oldest source), a-old at t=100, a-new at
        // t=200. Total 9 KiB of the 10 KiB quota.
        for (src, track, at, seed) in [
            (b, "b-1", 50u64, 1u8),
            (a, "a-old", 100, 2),
            (a, "a-new", 200, 3),
        ] {
            let server = FakeServer::serving(payload(1024, seed));
            let media = key(src, track);
            engine.advance_clock(at);
            assert_eq!(engine.admit(media.clone(), labels(track), None), Ok(()));
            // Swap the backend for the per-track payload.
            engine.backend = server;
            assert_eq!(
                drive_until_terminal(&mut engine, &media),
                JobState::Committed
            );
        }
        assert_eq!(engine.board().committed_bytes, 3 * 1024);

        // A 9 KiB download forces eviction: b first (oldest source), then
        // a-new (newest within a), leaving a-old intact.
        let big_payload = payload(9 * 1024, 9);
        engine.backend = FakeServer::serving(big_payload.clone());
        let big = key(a, "big");
        engine.advance_clock(300);
        assert_eq!(engine.admit(big.clone(), labels("big"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &big), JobState::Committed);
        assert_eq!(
            engine.catalogue(&key(b, "b-1")),
            OfflineCatalogueEntry::LiveOnly,
            "oldest source evicted first"
        );
        assert_eq!(
            engine.catalogue(&key(a, "a-new")),
            OfflineCatalogueEntry::LiveOnly,
            "newest within the walked source is surrendered first"
        );
        assert!(matches!(
            engine.catalogue(&key(a, "a-old")),
            OfflineCatalogueEntry::Cached(_)
        ));
        // The evicted files are gone; the survivor and the new commit remain.
        assert_eq!(
            std::fs::read(&committed_snapshot(&engine, &big).cache_path).unwrap(),
            big_payload
        );
    }

    #[test]
    fn a_file_larger_than_the_whole_quota_fails_quota_exceeded_terminally() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(2 * 1024, 4)), 2 * 1024);
        let src = source(12);
        declared(&mut engine, src);
        let small = key(src, "fits");
        assert_eq!(engine.admit(small.clone(), labels("t"), None), Ok(()));
        assert_eq!(
            drive_until_terminal(&mut engine, &small),
            JobState::Committed
        );

        engine.backend = FakeServer::serving(payload(9 * 1024, 5));
        let huge = key(src, "huge");
        engine.advance_clock(500);
        assert_eq!(engine.admit(huge.clone(), labels("t"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &huge), JobState::Failed);
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == huge)
            .unwrap();
        assert_eq!(row.failure, Some(OfflineError::QuotaExceeded));
        assert_eq!(engine.catalogue(&huge), OfflineCatalogueEntry::LiveOnly);
        // Nothing half-promoted: only the committed row's bytes are charged.
        assert_eq!(engine.board().committed_bytes, 2 * 1024);
    }

    // -- logout ----------------------------------------------------------------

    #[test]
    fn logout_cancels_in_flight_jobs_but_committed_rows_survive() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(10 * 1024, 11)), QUOTA);
        let src = source(13);
        declared(&mut engine, src);
        let committed = key(src, "survivor");
        let in_flight = key(src, "victim");
        assert_eq!(
            engine.admit(committed.clone(), labels("survivor"), None),
            Ok(())
        );
        assert_eq!(
            drive_until_terminal(&mut engine, &committed),
            JobState::Committed
        );

        engine.backend = FakeServer::serving(payload(10 * 1024, 12)).failing_read_at(2);
        assert_eq!(
            engine.admit(in_flight.clone(), labels("victim"), None),
            Ok(())
        );
        assert_eq!(engine.drive(&in_flight), Some(JobState::Receiving));

        // Logout: the in-flight lease revokes; the committed row is not
        // touched (DAAP rule).
        engine.on_source_disconnected(&src);
        assert_eq!(
            engine.catalogue(&in_flight),
            OfflineCatalogueEntry::LiveOnly
        );
        assert!(matches!(
            engine.catalogue(&committed),
            OfflineCatalogueEntry::Cached(_)
        ));
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == in_flight)
            .unwrap();
        assert_eq!(row.state, JobState::Cancelled);
        assert!(row.cached.is_none());
        // The cancelled temp left nothing behind.
        let track_dir = engine.store.track_dir(&in_flight);
        let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
        assert!(leftovers.is_empty());
    }

    // -- cache deletion ----------------------------------------------------------

    #[test]
    fn user_cache_deletion_unlinks_bytes_releases_quota_and_clears_the_row() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(3 * 1024, 13)), QUOTA);
        let src = source(14);
        declared(&mut engine, src);
        let media = key(src, "deletable");
        assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
        assert_eq!(
            drive_until_terminal(&mut engine, &media),
            JobState::Committed
        );
        let path = committed_snapshot(&engine, &media).cache_path.clone();
        assert_eq!(engine.board().committed_bytes, 3 * 1024);

        assert!(engine.delete_cached(&media).unwrap());
        assert!(
            !std::path::Path::new(&path).exists(),
            "deletion unlinks the file"
        );
        assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
        assert_eq!(engine.board().committed_bytes, 0);
        assert!(
            !engine.delete_cached(&media).unwrap(),
            "second delete reports nothing to do"
        );
    }

    // -- source replacement ---------------------------------------------------------

    #[test]
    fn bumping_the_epoch_cancels_stale_jobs_but_keeps_committed_rows() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(4096, 14)), QUOTA);
        let src = source(15);
        declared(&mut engine, src);
        let settled = key(src, "settled");
        assert_eq!(engine.admit(settled.clone(), labels("t"), None), Ok(()));
        assert_eq!(
            drive_until_terminal(&mut engine, &settled),
            JobState::Committed
        );

        // 10 KiB spans several segments so the injected transient failure
        // pauses the job mid-transfer instead of completing it in one pass.
        engine.backend = FakeServer::serving(payload(10 * 1024, 15)).failing_read_at(2);
        let stale = key(src, "stale");
        assert_eq!(engine.admit(stale.clone(), labels("t"), None), Ok(()));
        assert_eq!(engine.drive(&stale), Some(JobState::Receiving));

        // Source replacement bumps the generation: the stale job cancels,
        // the committed row survives.
        engine.bump_epoch(&src);
        assert_eq!(engine.epoch_of(&src), 1);
        let row = engine
            .board()
            .rows
            .into_iter()
            .find(|row| row.media_key == stale)
            .unwrap();
        assert_eq!(row.state, JobState::Cancelled);
        assert!(matches!(
            engine.catalogue(&settled),
            OfflineCatalogueEntry::Cached(_)
        ));
    }

    // -- redaction ---------------------------------------------------------------

    #[test]
    fn board_projection_never_exposes_paths_urls_or_credentials() {
        let (mut engine, _dir) = engine(FakeServer::serving(payload(4096, 16)), QUOTA);
        let src = source(16);
        declared(&mut engine, src);
        let good = key(src, "clean");
        assert_eq!(engine.admit(good.clone(), labels("clean"), None), Ok(()));
        assert_eq!(
            drive_until_terminal(&mut engine, &good),
            JobState::Committed
        );

        engine.backend = FakeServer::serving(payload(4096, 17)).with_wrong_advertised_digest();
        let bad = key(src, "corrupt");
        assert_eq!(engine.admit(bad.clone(), labels("corrupt"), None), Ok(()));
        assert_eq!(drive_until_terminal(&mut engine, &bad), JobState::Failed);

        let board = engine.board();
        let rendered: Vec<String> = board
            .rows
            .iter()
            .map(|row| format!("{:?}{:?}{:?}", row.labels, row.state, row.failure))
            .collect();
        let joined = rendered.join("\n");
        for forbidden in ["http://", "https://", "token=", "Bearer ", "password="] {
            assert!(
                !joined.contains(forbidden),
                "projection leaked {forbidden:?}"
            );
        }
        // No on-disk path reaches the projection.
        let committed_path = committed_snapshot(&engine, &good).cache_path;
        assert!(!joined.contains(&committed_path));
        // Committed rows expose exactly the licence label, never text.
        let clean_row = board.rows.iter().find(|row| row.media_key == good).unwrap();
        assert_eq!(
            clean_row.cached.as_ref().unwrap().licence_label,
            OperationalLicence::SourceDeclared
        );
    }

    fn hex_of(digest: &[u8; 32]) -> String {
        use std::fmt::Write as _;
        digest
            .iter()
            .fold(String::with_capacity(digest.len() * 2), |mut out, byte| {
                let _unused = write!(out, "{byte:02x}");
                out
            })
    }
}
