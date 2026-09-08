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
use crate::local::write_authority::{
    CommitOutcome, ConflictPolicy, ConflictResolution, PreparedWriteTarget,
};
use crate::source_lifecycle::CancellationObserver;

/// One destination mutation the executor owns and must undo on rollback.
///
/// Rollback reverses what was actually published, never what the plan
/// predicted. A Preserve conflict publishes to a renamed sibling — removing
/// the planned path would destroy the pre-existing original — and an
/// Overwrite commit replaces an original that only a saved copy can restore.
#[derive(Clone, Debug, Eq, PartialEq)]
enum OwnedChange {
    /// A file the executor published at this path; rollback removes exactly
    /// this path. Fresh publishes and preserved siblings both land here.
    PublishedFile { relative_path: PathBuf },
    /// A pre-existing file the executor replaced; its bytes were saved at
    /// `backup_relative_path` before the replace. Rollback republishes the
    /// backup over `relative_path` and consumes the backup.
    ReplacedFile {
        relative_path: PathBuf,
        backup_relative_path: PathBuf,
    },
    /// A directory the executor created; it must be empty once every file
    /// inside it has been rolled back. Recorded only when the directory was
    /// provably absent immediately before creation.
    CreatedDirectory { relative_path: PathBuf },
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
            },
            None => OwnedChange::PublishedFile {
                relative_path: outcome.relative_path,
            },
        },
        ConflictResolution::Fresh | ConflictResolution::Preserved => OwnedChange::PublishedFile {
            relative_path: outcome.relative_path,
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
    /// with the same identity is not an error. When the directory was
    /// provably absent immediately before creation, the new directory is
    /// recorded for rollback.
    fn execute_create_directory(
        &self,
        relative: &Path,
        context: &mut RunContext<'_>,
    ) -> Result<(), TransferError> {
        let final_path = self.request.destination.root().join(relative);
        let absent = match std::fs::symlink_metadata(&final_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => {
                return Err(TransferError::io(
                    "failed to inspect destination directory",
                    error,
                ));
            }
            Ok(_) => false,
        };
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;
        match self
            .request
            .destination
            .create_relative_directory(relative, self.request.conflict_policy)
        {
            Ok(_) => {
                if absent {
                    context.committed.push(OwnedChange::CreatedDirectory {
                        relative_path: relative.to_path_buf(),
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
        self.request
            .source
            .validate()
            .map_err(|error| TransferError::authority(format!("source not current: {error}")))?;
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;
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
    /// resolve a post-plan destination appearance.
    fn copy_and_commit_staged(
        staged: PreparedWriteTarget,
        declared_bytes: u64,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut File,
    ) -> Result<CommitOutcome, StageFailure> {
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
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(StageFailure::Collision(error))
            }
            Err(error) => Err(StageFailure::Final(TransferError::CommitFailed {
                context: error.to_string(),
            })),
        }
    }

    /// Resolve a destination that appeared after planning on a fresh-planned
    /// stage. The publish refused to replace it; the policy decides what
    /// happens next. Skip and Fail are represented distinctly — a Skip-policy
    /// stage is skipped without committing anything, a Fail-policy stage
    /// rejects the run — while Preserve re-resolves exactly as the planner
    /// would have: one retry onto a disambiguated sibling, with the
    /// collision untouched.
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
            (ConflictPolicy::Preserve, ConflictResolution::Fresh) => {
                let staged = self
                    .request
                    .destination
                    .prepare_write_relative_file(destination_relative, ConflictPolicy::Preserve)
                    .map_err(|error| {
                        TransferError::io("failed to stage preserved sibling", error)
                    })?;
                Self::copy_and_commit_staged(
                    staged,
                    declared_bytes,
                    stage_index,
                    context,
                    source_file,
                )
                .map(Some)
                .map_err(|failure| match failure {
                    StageFailure::Final(error) => error,
                    StageFailure::Collision(error) => {
                        TransferError::io("preserved sibling publish collided", error)
                    }
                })
            }
            // An Overwrite-planned commit never collides (replace semantics
            // with a bound backup), and a planned Preserved sibling
            // collision is a genuine allocation failure.
            _ => Err(TransferError::io(
                "failed to commit destination file",
                collision,
            )),
        }
    }

    /// Flush the staged file and verify the copied byte count against the
    /// plan's declared size. A short or oversized copy is a corrupted
    /// transfer: the staged copy is discarded and the failure reported.
    fn flush_and_verify_size(
        staged: PreparedWriteTarget,
        declared_bytes: u64,
        copied: u64,
    ) -> Result<PreparedWriteTarget, TransferError> {
        staged
            .staged_file()
            .flush()
            .map_err(|error| TransferError::io("failed to flush staged file", error))?;
        if declared_bytes != 0 && copied != declared_bytes {
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

    /// Remove the saved originals of successful overwrite commits.
    ///
    /// A completed transfer supersedes every saved original: each backup is
    /// a full-size hidden sibling that would otherwise accumulate until
    /// destination storage is exhausted. Removal is best-effort — a leftover
    /// hidden backup is preferable to failing a transfer whose bytes are
    /// already published, matching the link-based publish's policy for its
    /// staged-leaf unlink. The backups also remain restorable on the failure
    /// path, which never reaches this method.
    fn discard_superseded_backups(&self, context: &RunContext<'_>) {
        for change in &context.committed {
            if let OwnedChange::ReplacedFile {
                backup_relative_path,
                ..
            } = change
            {
                let _ = self
                    .request
                    .destination
                    .remove_relative_file(backup_relative_path);
            }
        }
    }

    /// Reverse every owned change in reverse commit order, revalidating the
    /// destination authority before each reversal.
    ///
    /// Published files are removed exactly where they were published — a
    /// preserved sibling is removed by its own name, never by the planned
    /// destination. Replaced files are restored from their saved backups.
    /// Created directories are removed deepest-first after their files are
    /// gone; an unexpected residue fails the rollback rather than deleting
    /// data the executor does not own.
    fn rollback(&self, context: &mut RunContext<'_>) -> Result<(), TransferError> {
        while let Some(change) = context.committed.pop() {
            self.request.destination.validate().map_err(|error| {
                TransferError::authority(format!("destination not current: {error}"))
            })?;
            match change {
                OwnedChange::PublishedFile { relative_path } => {
                    self.request
                        .destination
                        .remove_relative_file(&relative_path)
                        .map_err(|error| TransferError::RollbackFailed {
                            path: relative_path,
                            context: error.to_string(),
                        })?;
                }
                OwnedChange::ReplacedFile {
                    relative_path,
                    backup_relative_path,
                } => {
                    self.request
                        .destination
                        .restore_relative_file(&backup_relative_path, &relative_path)
                        .map_err(|error| TransferError::RollbackFailed {
                            path: relative_path,
                            context: error.to_string(),
                        })?;
                }
                OwnedChange::CreatedDirectory { relative_path } => {
                    self.request
                        .destination
                        .remove_relative_directory(&relative_path)
                        .map_err(|error| TransferError::RollbackFailed {
                            path: relative_path,
                            context: error.to_string(),
                        })?;
                }
            }
        }
        Ok(())
    }
}

/// Map an I/O error carrying the cancellation interrupt into the typed
/// cancelled error, everything else into a transfer I/O error.
fn transfer_io(context: &'static str, error: io::Error) -> TransferError {
    if error.kind() == io::ErrorKind::Interrupted {
        TransferError::Cancelled
    } else {
        TransferError::io(context, error)
    }
}

/// Roll a staged file back and translate its copy-phase failure. The
/// staged sibling is discarded so a failed copy never leaves litter in
/// the destination directory.
fn discard_staged_copy(
    staged: PreparedWriteTarget,
    context: &'static str,
    error: io::Error,
) -> TransferError {
    let _ = staged.rollback();
    transfer_io(context, error)
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
