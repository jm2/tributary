//! The engine's drive-loop phase steps and retry/restart budget internals.
//!
//! Each phase runs as its own step method so the per-job borrow always ends
//! before a multi-field `&mut self` helper runs; [`super::OfflineEngine::drive`]
//! dispatches on the typed state.

use crate::architecture::identity::MediaKey;
use crate::architecture::offline::{
    EntityValidator, JobState, LeaseId, OfflineError, OperationalLicence,
};

use crate::offline::storage::{size_on_disk, PublishCheck};

use super::{
    DriveStep, OfflineEngine, RangeOutcome, TransferBackend, MAX_ENTITY_RESTARTS,
    MAX_NETWORK_RETRIES, SEGMENT_BYTES,
};

impl<B: TransferBackend> OfflineEngine<B> {
    /// Connect phase: open the transfer, capture the validator, digest tier,
    /// and live total, and mint the temp reservation.
    pub(super) fn step_connect(&mut self, key: &MediaKey) -> DriveStep {
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
    pub(super) fn step_receive(&mut self, key: &MediaKey) -> DriveStep {
        let plan = match self.receive_plan(key) {
            ReceiveGate::Advance => return DriveStep::Advance,
            ReceiveGate::Fail(err) => return DriveStep::Fail(err),
            ReceiveGate::Plan(plan) => plan,
        };
        // Resume discipline: a job without a captured validator restarts
        // fully; a job with one truncates any torn tail back to the
        // journaled offset. The journaled offset is the only trusted resume
        // state — the raw on-disk length never is. Applied exactly once per
        // pause, then the flag clears so continuous segments flow freely.
        if let DriveStep::Fail(err) = self.apply_resume_discipline(key, &plan) {
            return DriveStep::Fail(err);
        }
        // The read offset is the job's journaled position after the
        // discipline above (a validator-less resume has just reset it to 0).
        let start = self.jobs.get(key).map_or(0, |job| job.record.current_bytes);
        let outcome = self
            .backend
            .read_range(key, plan.validator.as_ref(), start, plan.want);
        match outcome {
            Ok(RangeOutcome::Partial(bytes)) => {
                self.write_received_segment(key, start, plan.want, bytes)
            }
            Ok(RangeOutcome::EntityChanged) => DriveStep::Changed,
            Err(err) => DriveStep::Fail(err),
        }
    }

    /// Verify phase: resolve the digest check, enforce quota, publish.
    pub(super) fn step_verify(&mut self, key: &MediaKey) {
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

    // -- receive internals -------------------------------------------------

    /// Gather one receive step's inputs under a short job borrow.
    fn receive_plan(&mut self, key: &MediaKey) -> ReceiveGate {
        let Some(job) = self.jobs.get_mut(key) else {
            return ReceiveGate::Advance;
        };
        if job.record.state != JobState::Receiving {
            return ReceiveGate::Advance;
        }
        let want = match job.total_bytes {
            Some(total) => total
                .saturating_sub(job.record.current_bytes)
                .min(SEGMENT_BYTES),
            None => SEGMENT_BYTES,
        };
        if want == 0 {
            job.record.state = JobState::Verifying;
            return ReceiveGate::Advance;
        }
        if job.reservation.is_none() {
            return ReceiveGate::Fail(OfflineError::StorageUnavailable);
        }
        ReceiveGate::Plan(ReceivePlan {
            want,
            journaled: job.record.current_bytes,
            validator: job.record.resume_validator.clone(),
            resume_possible: job.record.resume_validator.is_some(),
            resuming: job.resume_pending,
        })
    }

    /// Apply the one-shot resume discipline for a paused receiving job:
    /// trim any torn tail (or reset a validator-less job to zero) so the
    /// next read starts at the trusted journaled offset. `DriveStep::Advance`
    /// means the discipline found nothing to correct.
    fn apply_resume_discipline(&mut self, key: &MediaKey, plan: &ReceivePlan) -> DriveStep {
        if !plan.resuming {
            return DriveStep::Advance;
        }
        let journaled_len = if plan.resume_possible {
            plan.journaled
        } else {
            0
        };
        let Some(job) = self.jobs.get_mut(key) else {
            return DriveStep::Advance;
        };
        job.resume_pending = false;
        if !plan.resume_possible && plan.journaled > 0 {
            job.record.current_bytes = 0;
        }
        let Some(reservation) = job.reservation.as_ref() else {
            return DriveStep::Fail(OfflineError::StorageUnavailable);
        };
        let disk_len = size_on_disk(reservation.temp_path());
        let trimmed = if disk_len > journaled_len {
            self.store
                .truncate_temp(reservation, journaled_len)
                .is_err()
        } else {
            disk_len < journaled_len
        };
        if trimmed {
            DriveStep::Fail(OfflineError::StorageUnavailable)
        } else {
            DriveStep::Advance
        }
    }

    /// Journal one received segment and advance on transfer completion.
    fn write_received_segment(
        &mut self,
        key: &MediaKey,
        start: u64,
        want: u64,
        bytes: Vec<u8>,
    ) -> DriveStep {
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

    // -- budget internals ---------------------------------------------------

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
    pub(super) fn retry_or_fail(&mut self, key: &MediaKey, err: OfflineError) -> bool {
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

    pub(super) fn fail(&mut self, key: &MediaKey, err: OfflineError) {
        if let Some(job) = self.jobs.get_mut(key) {
            job.record.state = JobState::Failed;
            job.record.failure = Some(err);
            job.record.last_lease = None;
        }
        self.release_reservation(key);
    }

    pub(super) fn restart_from_zero(&mut self, key: &MediaKey) {
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
}

/// Inputs one receive step gathers under a short job borrow.
struct ReceivePlan {
    want: u64,
    journaled: u64,
    validator: Option<EntityValidator>,
    resume_possible: bool,
    resuming: bool,
}

/// Pre-flight outcome of one receive step.
enum ReceiveGate {
    /// Nothing to do: no job, wrong state, or the transfer is complete.
    Advance,
    /// A storage precondition is broken; terminal for the job.
    Fail(OfflineError),
    /// Proceed with the planned segment read.
    Plan(ReceivePlan),
}
