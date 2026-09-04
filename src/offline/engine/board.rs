//! The credential-free board projection and commit-time quota enforcement.
//!
//! [`OfflineEngine::board`] renders the aggregate UI projection the GTK
//! storage panel shows; [`OfflineEngine::enforce_quota_then_publish`] is the
//! single commit point where quota is measured, eviction runs, and the
//! snapshot publishes.

use crate::architecture::identity::MediaKey;
use crate::architecture::offline::{
    JobState, OfflineCatalogueEntry, OfflineError, OperationalLicence,
};

use crate::offline::quota::{next_eviction_victim, EvictionCandidate};
use crate::offline::storage::{size_on_disk, PublishCheck};

use super::{
    CachedRowView, OfflineBoard, OfflineEngine, OfflineRowLabels, OfflineRowSnapshot,
    TransferBackend,
};

impl<B: TransferBackend> OfflineEngine<B> {
    /// Render the credential-free board projection the storage panel shows.
    #[must_use]
    pub fn board(&self) -> OfflineBoard {
        let mut keys = self.board_keys();
        keys.sort_by(|left, right| left.track_id.as_str().cmp(right.track_id.as_str()));
        let rows = keys
            .into_iter()
            .map(|key| {
                let cached_view = self.cached_view_for(&key);
                self.row_snapshot(key, cached_view)
            })
            .collect();
        OfflineBoard {
            rows,
            committed_bytes: self.ledger.committed_bytes(),
            quota_bytes: self.ledger.quota_bytes(),
        }
    }

    // -- board internals ---------------------------------------------------

    /// Every key the board shows: live job keys plus catalogue-only keys.
    fn board_keys(&self) -> Vec<MediaKey> {
        let mut keys: Vec<MediaKey> = self.jobs.keys().cloned().collect();
        for snapshot in self.catalog.all_snapshots() {
            if !self.jobs.contains_key(&snapshot.media_key) {
                keys.push(snapshot.media_key.clone());
            }
        }
        keys
    }

    /// The cached-bytes view for one key, `None` when live-only.
    fn cached_view_for(&self, key: &MediaKey) -> Option<CachedRowView> {
        match self.catalog.resolve(key) {
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
        }
    }

    /// Assemble one row snapshot from the job (when present) and the
    /// catalogue view.
    fn row_snapshot(
        &self,
        key: MediaKey,
        cached_view: Option<CachedRowView>,
    ) -> OfflineRowSnapshot {
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
    }

    // -- commit internals ----------------------------------------------------

    /// Quota enforcement happens exactly once, at the commit point: evict
    /// (transactionally — row deleted and file unlinked in one step) until
    /// the new snapshot fits, else fail the job `QuotaExceeded` terminally.
    pub(super) fn enforce_quota_then_publish(
        &mut self,
        key: &MediaKey,
        check: PublishCheck,
        licence: OperationalLicence,
        committed_at: u64,
    ) -> Result<(), OfflineError> {
        // Measure the received bytes so eviction targets the real size.
        let total = self.measure_received(key)?;
        if total > self.ledger.quota_bytes() {
            // No amount of eviction can ever make this file fit: fail the
            // job without destroying any committed row.
            self.fail(key, OfflineError::QuotaExceeded);
            return Err(OfflineError::QuotaExceeded);
        }
        if !self.ledger.admits(total) {
            self.evict_until_fits(key, total);
        }
        if !self.ledger.admits(total) {
            self.fail(key, OfflineError::QuotaExceeded);
            return Err(OfflineError::QuotaExceeded);
        }
        self.publish_snapshot(key, check, licence, committed_at)
    }

    /// The received byte count on disk for the job's temp reservation.
    fn measure_received(&self, key: &MediaKey) -> Result<u64, OfflineError> {
        let job = self.jobs.get(key).ok_or(OfflineError::StorageUnavailable)?;
        let reservation = job
            .reservation
            .as_ref()
            .ok_or(OfflineError::StorageUnavailable)?;
        Ok(size_on_disk(reservation.temp_path()))
    }

    /// Transactionally evict committed snapshots (oldest source first) until
    /// `total` fits, or no victim remains that can be unlinked.
    fn evict_until_fits(&mut self, key: &MediaKey, total: u64) {
        while !self.ledger.admits(total) {
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
                return;
            };
            let removed = self.delete_cached(&victim.key).unwrap_or(false);
            if !removed {
                // The victim would not unlink; nothing else will fit.
                return;
            }
        }
    }

    /// Verify the staged bytes against `check`, atomically publish, charge
    /// the ledger, and settle the job. Any verification failure is terminal.
    fn publish_snapshot(
        &mut self,
        key: &MediaKey,
        check: PublishCheck,
        licence: OperationalLicence,
        committed_at: u64,
    ) -> Result<(), OfflineError> {
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
