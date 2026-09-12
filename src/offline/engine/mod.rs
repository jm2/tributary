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
//!
//! Module layout: [`machine`] holds the drive-loop phase steps and the
//! retry/restart budget internals; [`board`] renders the credential-free
//! UI projection and owns commit-time quota enforcement; [`tests`] drives
//! the full state machine through a programmable fake backend.

use std::collections::HashMap;

use crate::architecture::identity::{MediaKey, SourceId};
use crate::architecture::offline::{
    JobRecord, JobState, OfflineCatalogueEntry, OfflineError, OperationalLicence,
};

use super::catalog::OfflineCatalog;
use super::quota::QuotaLedger;
use super::storage::{CacheStore, TempReservation};

mod board;
mod machine;
#[cfg(test)]
mod tests;

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
pub(super) const MAX_DRIVE_STEPS: usize = 64;

/// Outcome of one receive-phase step.
pub(super) enum DriveStep {
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

pub(super) struct ActiveJob {
    pub(super) record: JobRecord,
    pub(super) labels: OfflineRowLabels,
    pub(super) reservation: Option<TempReservation>,
    pub(super) network_retries: u32,
    pub(super) entity_restarts: u32,
    pub(super) total_bytes: Option<u64>,
    pub(super) advertised_digest: Option<[u8; 32]>,
    /// Set when a transient failure paused a receiving job: the next
    /// receive step must apply resume discipline (torn-tail trim or
    /// validator-less full restart) before reading the remainder.
    pub(super) resume_pending: bool,
}

impl ActiveJob {
    pub(super) fn terminal(&self) -> bool {
        self.record.state.is_terminal()
    }
}

/// The single-owner download/cache supervisor.
pub struct OfflineEngine<B: TransferBackend> {
    pub(super) backend: B,
    pub(super) store: CacheStore,
    pub(super) ledger: QuotaLedger,
    pub(super) catalog: OfflineCatalog,
    pub(super) jobs: HashMap<MediaKey, ActiveJob>,
    sources: HashMap<SourceId, SourceOfflinePosition>,
    epochs: HashMap<SourceId, u64>,
    pub(super) nonce: u64,
    pub(super) now_epoch_secs: u64,
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
        let snapshot = match self.catalog.resolve(key) {
            OfflineCatalogueEntry::Cached(snapshot) | OfflineCatalogueEntry::Revoked(snapshot) => {
                snapshot
            }
            OfflineCatalogueEntry::LiveOnly => return Ok(false),
        };
        // The unlink is the only fallible step: the row and its charge
        // stay until it succeeds, so a failed delete remains retryable
        // instead of stranding an undeletable, quota-charged row.
        self.store.unlink_snapshot(&snapshot)?;
        self.catalog.remove(key);
        self.ledger.release(snapshot.byte_size);
        Ok(true)
    }

    /// The offline catalogue read for one media key.
    #[must_use]
    pub fn catalogue(&self, key: &MediaKey) -> OfflineCatalogueEntry {
        self.catalog.resolve(key)
    }
}
