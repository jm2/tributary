//! The transfer executor.
//!
//! Holds the authorities and the plan; runs the stages in order; rolls back
//! committed stages on failure or cancellation. Rollback is outcome-driven:
//! the executor records what each stage *actually* published at the
//! destination (the planned path, a preserved sibling, or a replacement
//! backed by a saved original) and reverses exactly those owned changes.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::Path;

use super::types::{
    Stage, TransferError, TransferPlan, TransferProgress, TransferRequest, TransferSummary,
};
use crate::local::write_authority::{
    CommitError, CommitOutcome, ConflictPolicy, ConflictResolution, PreparedWriteTarget,
};
use crate::source_lifecycle::CancellationObserver;

mod rollback;

#[cfg(all(test, windows))]
mod displaced_only_tests;

use self::rollback::{discard_staged_copy, owned_change_for_copy, OwnedChange};

/// A copy-stage failure. A no-replace publish that refused a destination
/// which appeared after planning is distinguishable from every other
/// failure, so the policy can still resolve the post-plan collision
/// distinctly; everything else is already translated to the typed
/// transfer error.
enum StageFailure {
    /// The no-replace publish refused an existing destination.
    Collision(io::Error),
    /// A final failure; the staged copy was already discarded.
    Final(TransferError),
}

/// How many times a Preserve resolution may re-allocate onto the next
/// available sibling before the contention is reported as a failure. A
/// bound of several rounds absorbs concurrent Preserve transfers racing
/// the same disambiguated names without spinning. `pub(super)` so the
/// adversarial collision tests bind to the real bound instead of a
/// drift-prone literal.
pub(super) const PRESERVE_REALLOCATION_ATTEMPTS: usize = 8;

/// The transfer executor. Holds the authorities and the plan; runs the
/// stages in order; rolls back on failure or cancellation.
pub struct TransferExecutor {
    request: TransferRequest,
    plan: TransferPlan,
}

/// Mutable state shared by every stage runner of one [`TransferExecutor`]
/// run: the progress sink, the cancellation observer, the running byte
/// count, and the owned destination changes eligible for rollback.
struct RunContext<'a> {
    progress: &'a mut dyn TransferProgress,
    cancellation: &'a CancellationObserver,
    bytes_so_far: u64,
    total_bytes: u64,
    total_stages: u32,
    committed: Vec<OwnedChange>,
}

impl TransferExecutor {
    /// Construct an executor from a previously planned request.
    pub fn new(request: TransferRequest, plan: TransferPlan) -> Self {
        Self { request, plan }
    }

    /// Run the plan to completion, reporting progress through `progress`,
    /// observing `cancellation` between stages and between copy chunks, and
    /// rolling back every committed stage on any error or cancellation.
    ///
    /// Every unsuccessful exit — a failed stage, a mid-copy cancellation, a
    /// lost authority — is routed through the checked rollback before the
    /// error is surfaced. When the rollback itself fails, its error is
    /// reported instead of the original cause: a destination left dirty is
    /// the more severe condition and must not be masked.
    pub fn run(
        self,
        progress: &mut dyn TransferProgress,
        cancellation: &CancellationObserver,
    ) -> Result<TransferSummary, TransferError> {
        let mut context = RunContext {
            progress,
            cancellation,
            bytes_so_far: 0,
            total_bytes: self.plan.total_bytes(),
            total_stages: self.plan.stage_count(),
            committed: Vec::new(),
        };
        let mut committed_stages: u32 = 0;
        match self.execute_plan(&mut context, &mut committed_stages) {
            Ok(()) => {
                // A completed transfer supersedes every saved original, but
                // a backup name a concurrent writer swapped for a foreign
                // object is refused — never silently deleted — and the
                // refusal surfaces as a transfer failure: the destination
                // namespace is not in the state the successful summary
                // would claim.
                self.discard_superseded_backups(&context)?;
                Ok(TransferSummary {
                    committed_stages,
                    bytes_copied: context.bytes_so_far,
                    completed: true,
                })
            }
            Err(error) => {
                self.rollback(&mut context)?;
                Err(error)
            }
        }
    }

    /// Run every planned stage in order. Errors and cancellations return
    /// directly; the caller owns rollback.
    fn execute_plan(
        &self,
        context: &mut RunContext<'_>,
        committed_stages: &mut u32,
    ) -> Result<(), TransferError> {
        for (index, stage) in self.plan.stages().iter().enumerate() {
            if context.cancellation.is_cancelled() {
                return Err(TransferError::Cancelled);
            }
            context
                .progress
                .on_stage_started(stage, index as u32, context.total_stages);
            let committed = self.run_stage(stage, index as u32, context)?;
            // A stage that was distinctly skipped (a destination that
            // appeared after planning under a Skip policy) finished without
            // committing anything and must not inflate the count.
            if committed {
                *committed_stages = committed_stages.saturating_add(1);
            }
            context.progress.on_stage_completed(
                stage,
                index as u32,
                context.total_stages,
                context.bytes_so_far,
                context.total_bytes,
            );
        }
        Ok(())
    }

    /// Execute one stage, recording the stage's actual owned destination
    /// changes for rollback. Returns whether the stage committed a change;
    /// a distinctly skipped stage reports `false`.
    fn run_stage(
        &self,
        stage: &Stage,
        index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<bool, TransferError> {
        match stage {
            Stage::CreateDirectory {
                destination_relative_path,
            } => self
                .execute_create_directory(destination_relative_path, context)
                .map(|()| true),
            Stage::CopyFile {
                source_relative_path,
                destination_relative_path,
                bytes,
                conflict,
                ..
            } => {
                let committed = self.execute_copy_file(
                    source_relative_path,
                    destination_relative_path,
                    *bytes,
                    *conflict,
                    index,
                    context,
                )?;
                let Some(outcome) = committed else {
                    // A destination that appeared between planning and
                    // execution under a Skip policy is skipped, distinctly:
                    // nothing was published and nothing is owned.
                    return Ok(false);
                };
                context.committed.push(owned_change_for_copy(outcome));
                Ok(true)
            }
            Stage::RemoveFile { .. } => {
                // RemoveFile stages are inserted only by the rollback path
                // and never appear in a forward plan. Skip defensively.
                Ok(false)
            }
        }
    }

    /// Create one destination directory. Idempotent: an existing directory
    /// with the same identity is not an error. Exactly the components the
    /// authority reports as created by this call — leaf and any ancestors it
    /// had to create along the way — are recorded for rollback, so a nested
    /// destination never leaves created ancestor directories behind after a
    /// reversal. Ownership is never inferred from a pre-creation absence
    /// scan: a component that already existed, including one a concurrent
    /// writer created moments before the creation call, is adopted rather
    /// than owned and must survive rollback. The recorded identity of each
    /// created component is the one the authority captured during the
    /// exclusive creation itself — never a post-hoc lookup of the path,
    /// which a newcomer replacing the just-created directory between
    /// creation and capture would poison into an owned record.
    fn execute_create_directory(
        &self,
        relative: &Path,
        context: &mut RunContext<'_>,
    ) -> Result<(), TransferError> {
        let final_path = self.request.destination.root().join(relative);
        ensure_normal_item_path(relative)?;
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;
        match self
            .request
            .destination
            .create_relative_directory(relative, self.request.conflict_policy)
        {
            Ok((_, created)) => {
                for created_directory in created {
                    context.committed.push(OwnedChange::CreatedDirectory {
                        created_directory: created_directory.identity,
                        relative_path: created_directory.relative_path,
                    });
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match std::fs::symlink_metadata(&final_path) {
                    Ok(metadata) if metadata.is_dir() => Ok(()),
                    _ => Err(TransferError::io(
                        "directory creation failed with AlreadyExists",
                        error,
                    )),
                }
            }
            Err(error) => Err(TransferError::io("create directory failed", error)),
        }
    }

    /// Validate both authorities, then copy one source file into a staged
    /// destination file and commit it atomically.
    ///
    /// The stage's planned conflict resolution is consumed verbatim by the
    /// write authority — the executor never re-decides a conflict. Returns
    /// `Ok(None)` when the stage is distinctly skipped: the destination
    /// appeared after planning, under a Skip policy. A Fail policy surfaces
    /// the post-plan collision as a typed rejection instead, and a Preserve
    /// policy re-allocates onto the next available sibling within a bounded
    /// loop — an allocation race on a freshly chosen hidden name is normal
    /// contention.
    fn execute_copy_file(
        &self,
        source_relative: &Path,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<Option<CommitOutcome>, TransferError> {
        self.validate_endpoints()?;
        let result = self
            .request
            .source
            .with_relative_file(source_relative, |mut source_file| {
                match self.stage_and_commit_file(
                    destination_relative,
                    declared_bytes,
                    planned_conflict,
                    stage_index,
                    context,
                    &mut source_file,
                ) {
                    Ok(committed) => Ok(Ok(committed)),
                    Err(StageFailure::Final(error)) => Ok(Err(error)),
                    Err(StageFailure::Collision(error)) => {
                        // The Preserve re-resolution re-copies the source;
                        // rewind it to the start first. A rewind failure is
                        // an I/O error of the copy source like any other.
                        source_file.rewind()?;
                        Ok(self.resolve_post_plan_collision(
                            destination_relative,
                            declared_bytes,
                            planned_conflict,
                            stage_index,
                            context,
                            &mut source_file,
                            error,
                        ))
                    }
                }
            });
        Self::translate_source_handle_error(result)
    }

    /// Verify that both transfer endpoints are still the authorities the
    /// request was planned against; a stale endpoint fails the stage
    /// fail-closed before any namespace access.
    fn validate_endpoints(&self) -> Result<(), TransferError> {
        self.request
            .source
            .validate()
            .map_err(|error| TransferError::authority(format!("source not current: {error}")))?;
        self.request
            .destination
            .validate()
            .map_err(|error| TransferError::authority(format!("destination not current: {error}")))
    }

    /// Flatten the source-handle closure outcome: the closure speaks in
    /// `TransferError` terms already, while a closure-level I/O error means
    /// the copy source itself failed — except `Interrupted`, which is the
    /// cooperative cancellation signal.
    fn translate_source_handle_error(
        result: io::Result<Result<Option<CommitOutcome>, TransferError>>,
    ) -> Result<Option<CommitOutcome>, TransferError> {
        match result {
            // The closure ran; its outcome speaks in TransferError terms.
            Ok(committed) => committed,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                Err(TransferError::Cancelled)
            }
            Err(error) => Err(TransferError::io("failed to copy source file", error)),
        }
    }

    /// Stage one destination file against the planned conflict resolution,
    /// stream the source into it, and commit. The staged target's `Drop`
    /// discards it on every failure path.
    fn stage_and_commit_file(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut File,
    ) -> Result<Option<CommitOutcome>, StageFailure> {
        let staged = self
            .request
            .destination
            .prepare_write_relative_file_with_resolution(destination_relative, planned_conflict)
            .map_err(|error| {
                StageFailure::Final(TransferError::io("failed to stage destination file", error))
            })?;
        Self::copy_and_commit_staged(staged, declared_bytes, stage_index, context, source_file)
            .map(Some)
    }

    /// Copy the source into `staged`, verify the byte count, and commit.
    /// The staged file is flushed and closed inside commit before the
    /// publish; a commit collision is reported distinctly so the policy can
    /// resolve a post-plan destination appearance. A publish that landed
    /// but failed its post-publish verification is reported as a final
    /// failure with the outcome recorded first, so rollback can undo the
    /// committed bytes.
    fn copy_and_commit_staged(
        staged: PreparedWriteTarget,
        declared_bytes: u64,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut File,
    ) -> Result<CommitOutcome, StageFailure> {
        // The running byte count must reflect only bytes that stayed
        // committed: a collision discards the staged copy (Skip) or forces
        // a re-copy (Preserve), so the pre-attempt count is captured before
        // any bytes are copied and restored before the collision is
        // resolved — otherwise a retry double-counts and a skip leaves
        // discarded bytes counted as copied. Each retry re-enters this
        // function with a fresh staged target, so the entry value is that
        // attempt's pre-attempt count.
        let bytes_before_attempt = context.bytes_so_far;
        let copied = match copy_in_chunks(source_file, &staged, stage_index, context) {
            Ok(copied) => copied,
            Err(error) => {
                return Err(StageFailure::Final(discard_staged_copy(
                    staged,
                    "failed to copy source file",
                    error,
                )));
            }
        };
        let staged = Self::flush_and_verify_size(staged, declared_bytes, copied)
            .map_err(StageFailure::Final)?;
        match staged.commit() {
            Ok(outcome) => Ok(outcome),
            Err(CommitError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                context.bytes_so_far = bytes_before_attempt;
                context.progress.on_bytes_copied(
                    stage_index,
                    context.total_stages,
                    context.bytes_so_far,
                    context.total_bytes,
                );
                Err(StageFailure::Collision(error))
            }
            Err(CommitError::Io(error)) => Err(StageFailure::Final(TransferError::CommitFailed {
                context: error.to_string(),
            })),
            Err(CommitError::PublishVerification { outcome, error }) => {
                // The publish reached a state that must be recorded: either
                // the staged bytes landed and only the post-publish mount
                // revalidation failed, or (Windows) the replace exhausted
                // its rebind bound with nothing of the transfer's published
                // and the displaced original retained at a backup. Distinguish
                // the two through the outcome's disposition so rollback
                // reverses (identity-verified) exactly what the commit left,
                // then fail the stage — a committed or displaced state must
                // never be left unrecorded behind a failure.
                context.committed.push(owned_change_for_copy(outcome));
                Err(StageFailure::Final(TransferError::CommitFailed {
                    context: error.to_string(),
                }))
            }
        }
    }

    /// Resolve a destination that appeared after planning on a fresh-planned
    /// stage, or a preserved sibling that an allocation race took. The
    /// publish refused to replace it; the policy decides what happens next.
    /// Skip and Fail are represented distinctly — a Skip-policy stage is
    /// skipped without committing anything, a Fail-policy stage rejects the
    /// run — Overwrite re-resolves exactly once as the planner would have,
    /// through [`Self::restage_and_commit`], and Preserve re-allocates onto
    /// the next available sibling within a bounded loop: an allocation race
    /// on a freshly chosen hidden sibling name is normal contention, not a
    /// failure, so a concurrent Preserve transfer winning the same name
    /// must never roll back this transfer.
    #[allow(clippy::too_many_arguments)]
    fn resolve_post_plan_collision(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut File,
        collision: io::Error,
    ) -> Result<Option<CommitOutcome>, TransferError> {
        match (self.request.conflict_policy, planned_conflict) {
            (ConflictPolicy::Skip, ConflictResolution::Fresh) => Ok(None),
            (ConflictPolicy::Fail, ConflictResolution::Fresh) => {
                Err(TransferError::ConflictRejected {
                    path: destination_relative.to_path_buf(),
                })
            }
            (policy @ ConflictPolicy::Overwrite, ConflictResolution::Fresh) => {
                Self::restage_and_commit(
                    self,
                    destination_relative,
                    declared_bytes,
                    stage_index,
                    context,
                    source_file,
                    policy,
                )
            }
            // Preserve: the collided name — the planned destination of a
            // fresh-planned stage, or a disambiguated sibling an
            // allocation race took — is re-allocated onto the next
            // available sibling and re-published, within a bounded loop.
            // Skip/Fail semantics are destination appearances and remain
            // handled above, exactly as before.
            (ConflictPolicy::Preserve, _) => self.reallocate_and_commit_preserved(
                destination_relative,
                declared_bytes,
                stage_index,
                context,
                source_file,
                collision,
            ),
            // A planned-Overwrite collision has no further policy
            // resolution: the replace loop already re-bound within its own
            // bound, so the collision surfaces as an ordinary I/O failure.
            _ => Err(TransferError::io(
                "failed to commit destination file",
                collision,
            )),
        }
    }

    /// Re-allocate a Preserve resolution onto the next available sibling
    /// and commit, retrying within a bounded loop. Every attempt stages a
    /// fresh sibling target, re-copies the source (rewound by the caller),
    /// and observes cancellation between attempts; a collided attempt's
    /// staged copy is discarded by its target's drop and its byte count
    /// was restored before the collision surfaced, so progress never
    /// double-counts and a failed attempt never leaves litter. Exhausting
    /// the bound surfaces the last collision as the typed I/O error: the
    /// sibling namespace is adversarially contested, not silently
    /// mis-resolved.
    fn reallocate_and_commit_preserved(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut File,
        first_collision: io::Error,
    ) -> Result<Option<CommitOutcome>, TransferError> {
        let mut last_collision = first_collision;
        for _ in 0..PRESERVE_REALLOCATION_ATTEMPTS {
            if context.cancellation.is_cancelled() {
                return Err(TransferError::Cancelled);
            }
            match self.stage_and_commit_file(
                destination_relative,
                declared_bytes,
                ConflictResolution::Preserved,
                stage_index,
                context,
                source_file,
            ) {
                Ok(Some(outcome)) => return Ok(Some(outcome)),
                // A Preserved resolution is never distinctly skipped; the
                // arm exists for shape only.
                Ok(None) => {}
                Err(StageFailure::Final(error)) => return Err(error),
                Err(StageFailure::Collision(error)) => {
                    last_collision = error;
                    // The next attempt re-copies the source from the start.
                    source_file
                        .rewind()
                        .map_err(|error| TransferError::io("failed to copy source file", error))?;
                }
            }
        }
        Err(TransferError::io(
            "preserved sibling allocation kept colliding with concurrent transfers",
            last_collision,
        ))
    }

    /// Re-stage the source against the Overwrite policy resolution and
    /// commit exactly once. The collided attempt's staged copy was
    /// discarded with its target, and its byte count was restored before
    /// the collision surfaced, so the retry re-copies the source into the
    /// fresh staged target exactly once. Under Overwrite the request's
    /// stated policy is to replace, so the retry binds the current
    /// occupant to a commit-time backup and swaps it out atomically. A
    /// second collision has no policy resolution and surfaces as an
    /// ordinary I/O failure.
    #[allow(clippy::too_many_arguments)]
    fn restage_and_commit(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut File,
        policy: ConflictPolicy,
    ) -> Result<Option<CommitOutcome>, TransferError> {
        let (stage_context, collision_context) = match policy {
            ConflictPolicy::Overwrite => (
                "failed to stage overwrite replacement",
                "overwrite replacement publish collided",
            ),
            _ => (
                "failed to stage preserved sibling",
                "preserved sibling publish collided",
            ),
        };
        let staged = self
            .request
            .destination
            .prepare_write_relative_file(destination_relative, policy)
            .map_err(|error| TransferError::io(stage_context, error))?;
        Self::copy_and_commit_staged(staged, declared_bytes, stage_index, context, source_file)
            .map(Some)
            .map_err(|failure| match failure {
                StageFailure::Final(error) => error,
                StageFailure::Collision(error) => TransferError::io(collision_context, error),
            })
    }

    /// Flush the staged file and verify the copied byte count against the
    /// plan's declared size. A short or oversized copy is a corrupted
    /// transfer: the staged copy is discarded and the failure reported.
    ///
    /// Zero is a valid declared size, not an unknown-size sentinel: a file
    /// planned as empty and grown before execution fails the comparison
    /// like any other mismatch, so a grown source can never smuggle bytes
    /// past a zero-byte capacity budget or skew progress totals.
    fn flush_and_verify_size(
        staged: PreparedWriteTarget,
        declared_bytes: u64,
        copied: u64,
    ) -> Result<PreparedWriteTarget, TransferError> {
        staged
            .staged_file()
            .flush()
            .map_err(|error| TransferError::io("failed to flush staged file", error))?;
        if copied != declared_bytes {
            return Err(discard_staged_copy(
                staged,
                "source size differs from declared size",
                io::Error::other(format!(
                    "source size {copied} differs from declared {declared_bytes} bytes"
                )),
            ));
        }
        Ok(staged)
    }
}

/// Stream `source` into the staged file in fixed-size chunks, reporting
/// progress after every chunk and checking cancellation between chunks.
fn copy_in_chunks(
    source: &mut dyn Read,
    staged: &PreparedWriteTarget,
    stage_index: u32,
    context: &mut RunContext<'_>,
) -> io::Result<u64> {
    const CHUNK: usize = 64 * 1024;
    let mut buffer = vec![0u8; CHUNK];
    let mut copied: u64 = 0;
    loop {
        if context.cancellation.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let read = source
            .read(&mut buffer)
            .map_err(|error| TransferError::io("failed to read source file", error))
            .map_err(io::Error::other)?;
        if read == 0 {
            break;
        }
        staged
            .staged_file()
            .write_all(&buffer[..read])
            .map_err(|error| TransferError::io("failed to write staged file", error))
            .map_err(io::Error::other)?;
        copied = copied.saturating_add(read as u64);
        context.bytes_so_far = context.bytes_so_far.saturating_add(read as u64);
        context.progress.on_bytes_copied(
            stage_index,
            context.total_stages,
            context.bytes_so_far,
            context.total_bytes,
        );
    }
    Ok(copied)
}

/// Validate that every component of `relative` is a normal (relative,
/// traversal-free) component, reporting the typed invalid-path error exactly
/// as the executor always has. Ownership is decided by the authority's
/// created-component report — never by a race-prone pre-creation absence
/// scan — so no filesystem walk happens here.
fn ensure_normal_item_path(relative: &Path) -> Result<(), TransferError> {
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(TransferError::InvalidItemPath {
                path: relative.to_path_buf(),
            });
        }
    }
    Ok(())
}
