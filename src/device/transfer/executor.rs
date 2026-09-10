//! The transfer executor.
//!
//! Holds the authorities and the plan; runs the stages in order; rolls back
//! committed stages on failure or cancellation. Rollback is outcome-driven:
//! the executor records what each stage *actually* published at the
//! destination (the planned path, a preserved sibling, or a replacement
//! backed by a saved original) and reverses exactly those owned changes.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use super::types::{
    Stage, TransferError, TransferPlan, TransferProgress, TransferRequest, TransferSummary,
};
use crate::local::root_authority::LeafIdentity;
use crate::local::write_authority::{
    CommitError, CommitOutcome, ConflictPolicy, ConflictResolution, PreparedWriteTarget,
};
use crate::source_lifecycle::CancellationObserver;

mod rollback;

use self::rollback::discard_staged_copy;

/// One destination mutation the executor owns and must undo on rollback.
///
/// Rollback reverses what was actually published, never what the plan
/// predicted. A Preserve conflict publishes to a renamed sibling — removing
/// the planned path would destroy the pre-existing original — and an
/// Overwrite commit replaces an original that only a saved copy can restore.
///
/// Every variant also carries the no-follow leaf identity the write
/// authority captured at publish/creation time. Before each reversal
/// mutation the destination is re-stat'ed and the identity compared: a
/// mismatch means a concurrent writer replaced the transfer's publication
/// after it committed, so reversing by pathname alone would delete — or
/// restore a backup over — a file the transfer does not own. Such a leaf is
/// refused and the rollback fails instead. `None` degrades that reversal to
/// the legacy path-only behavior (identity capture is best-effort).
#[derive(Clone, Debug, Eq, PartialEq)]
enum OwnedChange {
    /// A file the executor published at this path; rollback removes exactly
    /// this path. Fresh publishes and preserved siblings both land here.
    PublishedFile {
        relative_path: PathBuf,
        published_leaf: Option<LeafIdentity>,
    },
    /// A pre-existing file the executor replaced; its bytes were saved at
    /// `backup_relative_path` before the replace. Rollback republishes the
    /// backup over `relative_path` and consumes the backup.
    ReplacedFile {
        relative_path: PathBuf,
        backup_relative_path: PathBuf,
        published_leaf: Option<LeafIdentity>,
    },
    /// A directory (or directory ancestor) the executor created; it must be
    /// empty once every file inside it has been rolled back. Recorded only
    /// when the directory was provably absent immediately before creation.
    CreatedDirectory {
        relative_path: PathBuf,
        created_directory: Option<LeafIdentity>,
    },
}

/// Classify a committed copy into the owned change rollback must reverse.
///
/// The commit outcome is authoritative about what the publish actually did:
/// an Overwrite commit that bound a replaced original (`replaced_original`)
/// is rolled back by restoring that backup, while an Overwrite commit that
/// replaced nothing (the occupant vanished before the publish), a fresh
/// publish, and a preserved sibling all roll back by removing the actual
/// published path.
fn owned_change_for_copy(outcome: CommitOutcome) -> OwnedChange {
    match outcome.resolution {
        ConflictResolution::Overwrite => match outcome.replaced_original {
            Some(backup_relative_path) => OwnedChange::ReplacedFile {
                relative_path: outcome.relative_path,
                backup_relative_path,
                published_leaf: outcome.published_leaf,
            },
            None => OwnedChange::PublishedFile {
                relative_path: outcome.relative_path,
                published_leaf: outcome.published_leaf,
            },
        },
        ConflictResolution::Fresh | ConflictResolution::Preserved => OwnedChange::PublishedFile {
            relative_path: outcome.relative_path,
            published_leaf: outcome.published_leaf,
        },
    }
}

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
                self.discard_superseded_backups(&context);
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
    /// than owned and must survive rollback.
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
                        created_directory: self
                            .request
                            .destination
                            .relative_leaf_identity(&created_directory),
                        relative_path: created_directory,
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
    /// policy re-resolves once to a disambiguated sibling.
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
                // The publish happened; only the post-publish mount
                // revalidation failed. Record the owned change so the outer
                // rollback reverses (identity-verified) exactly what landed,
                // then fail the stage — committed bytes must never be left
                // unrecorded behind a failure.
                context.committed.push(owned_change_for_copy(outcome));
                Err(StageFailure::Final(TransferError::CommitFailed {
                    context: error.to_string(),
                }))
            }
        }
    }

    /// Resolve a destination that appeared after planning on a fresh-planned
    /// stage. The publish refused to replace it; the policy decides what
    /// happens next. Skip and Fail are represented distinctly — a Skip-policy
    /// stage is skipped without committing anything, a Fail-policy stage
    /// rejects the run — while Preserve and Overwrite re-resolve exactly as
    /// the planner would have, through [`Self::restage_and_commit`].
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
            (
                policy @ (ConflictPolicy::Overwrite | ConflictPolicy::Preserve),
                ConflictResolution::Fresh,
            ) => Self::restage_and_commit(
                self,
                destination_relative,
                declared_bytes,
                stage_index,
                context,
                source_file,
                policy,
            ),
            // A planned Preserved sibling collision is a genuine allocation
            // failure: the disambiguated name is freshly chosen, so an
            // occupant there can only be an allocation race the policy has
            // no further resolution for.
            _ => Err(TransferError::io(
                "failed to commit destination file",
                collision,
            )),
        }
    }

    /// Re-stage the source against a post-plan policy resolution and commit
    /// exactly once. The collided attempt's staged copy was discarded with
    /// its target, and its byte count was restored before the collision
    /// surfaced, so the retry re-copies the source into the fresh staged
    /// target exactly once. Under Overwrite the request's stated policy is
    /// to replace, so the retry binds the current occupant to a commit-time
    /// backup and swaps it out atomically; under Preserve it re-resolves
    /// onto a disambiguated sibling with the collision untouched. A second
    /// collision has no policy resolution and surfaces as an ordinary I/O
    /// failure.
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
