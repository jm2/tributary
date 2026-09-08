//! The transfer executor.
//!
//! Holds the authorities and the plan; runs the stages in order; rolls back
//! committed stages on failure or cancellation. Rollback is outcome-driven:
//! the executor records what each stage *actually* published at the
//! destination (the planned path, a preserved sibling, or a replacement
//! backed by a saved original) and reverses exactly those owned changes.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use uuid::Uuid;

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

/// What one committed copy stage produced at the destination.
struct CommittedCopy {
    outcome: CommitOutcome,
    /// Restorable original of a replaced destination, when the commit
    /// overwrote a pre-existing file.
    backup: Option<PathBuf>,
}

/// Classify a committed copy into the owned change rollback must reverse.
///
/// An Overwrite commit with a saved original is rolled back by restoring
/// that original; an Overwrite commit that published where nothing
/// pre-existed, a fresh publish, and a preserved sibling all roll back by
/// removing the actual published path.
fn owned_change_for_copy(committed: CommittedCopy) -> OwnedChange {
    match committed.outcome.resolution {
        ConflictResolution::Overwrite => match committed.backup {
            Some(backup_relative_path) => OwnedChange::ReplacedFile {
                relative_path: committed.outcome.relative_path,
                backup_relative_path,
            },
            None => OwnedChange::PublishedFile {
                relative_path: committed.outcome.relative_path,
            },
        },
        ConflictResolution::Fresh | ConflictResolution::Preserved => OwnedChange::PublishedFile {
            relative_path: committed.outcome.relative_path,
        },
    }
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
                let Some(committed) = committed else {
                    // A destination that appeared between planning and
                    // execution under a Skip policy is skipped, distinctly:
                    // nothing was published and nothing is owned.
                    return Ok(false);
                };
                context.committed.push(owned_change_for_copy(committed));
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
    /// Returns `Ok(None)` when the stage is distinctly skipped: the
    /// destination was absent at planning but appeared since, under a Skip
    /// policy. A Fail policy surfaces the post-plan collision as a typed
    /// rejection instead.
    fn execute_copy_file(
        &self,
        source_relative: &Path,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<Option<CommittedCopy>, TransferError> {
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
                Ok(self.stage_and_commit_file(
                    destination_relative,
                    declared_bytes,
                    planned_conflict,
                    stage_index,
                    context,
                    &mut source_file,
                ))
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

    /// Stage one destination file, stream the source into it, save a
    /// restorable original when the commit will replace an existing file,
    /// and commit.
    ///
    /// A cancelled or short copy rolls the staged file back; a flush or
    /// commit failure is cleaned up by the staged target's `Drop`. The
    /// returned backup path is set only when an Overwrite commit actually
    /// replaced a pre-existing original and must be recorded as owned by
    /// the caller: rollback restores it over the destination.
    fn stage_and_commit_file(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut dyn Read,
    ) -> Result<Option<CommittedCopy>, TransferError> {
        let staged = match self
            .request
            .destination
            .prepare_write_relative_file(destination_relative, self.request.conflict_policy)
        {
            Ok(staged) => staged,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // The destination existed at staging but not at planning.
                // Skip and Fail are represented distinctly; Preserve and
                // Overwrite re-resolve naturally inside prepare.
                return post_plan_collision_outcome(
                    self.request.conflict_policy,
                    planned_conflict,
                    destination_relative,
                    error,
                );
            }
            Err(error) => {
                return Err(TransferError::io("failed to stage destination file", error));
            }
        };
        let copied = match copy_in_chunks(source_file, &staged, stage_index, context) {
            Ok(copied) => copied,
            Err(error) => {
                return Err(discard_staged_copy(
                    staged,
                    "failed to copy source file",
                    error,
                ));
            }
        };
        let staged = Self::flush_and_verify_size(staged, declared_bytes, copied)?;
        self.save_backup_and_commit(staged, destination_relative, context)
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

    /// Save the restorable original for an Overwrite commit and publish.
    ///
    /// An Overwrite commit replaces a pre-existing original. The original is
    /// saved through the retained authority first so a later rollback can
    /// put it back; the original stays in place until the replace publish,
    /// so a mid-copy cancellation never disturbs it. An absent destination
    /// has nothing to save — the commit publishes fresh and rollback removes
    /// the published file outright.
    fn save_backup_and_commit(
        &self,
        staged: PreparedWriteTarget,
        destination_relative: &Path,
        context: &RunContext<'_>,
    ) -> Result<Option<CommittedCopy>, TransferError> {
        let backup = if staged.resolution() == ConflictResolution::Overwrite {
            match self.save_original_backup(destination_relative, context) {
                Ok(backup) => backup,
                Err(error) => {
                    let _ = staged.rollback();
                    return Err(error);
                }
            }
        } else {
            None
        };
        match staged.commit() {
            Ok(outcome) => Ok(Some(CommittedCopy { outcome, backup })),
            // The publish failed, so the backup was never needed: remove it
            // so the destination parent is not polluted with a hidden copy
            // of the original.
            Err(error) => Err(self.failed_commit_discards_backup(backup, error)),
        }
    }

    /// A failed publish never needed its saved original: remove the backup
    /// sibling so the destination parent is not polluted with a hidden copy
    /// of the original, then translate the commit failure.
    fn failed_commit_discards_backup(
        &self,
        backup: Option<PathBuf>,
        error: io::Error,
    ) -> TransferError {
        if let Some(backup) = &backup {
            let _ = self.request.destination.remove_relative_file(backup);
        }
        TransferError::CommitFailed {
            context: error.to_string(),
        }
    }

    /// Copy the current bytes of the destination file into a hidden backup
    /// sibling through the retained authority. Returns the backup's
    /// relative path, or `None` when the destination did not exist (an
    /// overwrite onto an absent name has no original to restore).
    fn save_original_backup(
        &self,
        destination_relative: &Path,
        context: &RunContext<'_>,
    ) -> Result<Option<PathBuf>, TransferError> {
        let backup_relative = backup_sibling_path(destination_relative);
        let backup = self
            .request
            .destination
            .prepare_write_relative_file(&backup_relative, ConflictPolicy::Fail)
            .map_err(|error| TransferError::io("failed to stage original backup file", error))?;
        let saved = self.request.destination.mount().with_relative_file(
            destination_relative,
            |mut original| {
                copy_in_chunks_quiet(&mut original, backup.staged_file(), context.cancellation)?;
                backup.staged_file().flush()
            },
        );
        if let Err(error) = saved {
            let _ = backup.rollback();
            if error.kind() == io::ErrorKind::NotFound {
                // No pre-existing original: the overwrite will publish
                // fresh and owns only what it publishes.
                return Ok(None);
            }
            return Err(transfer_io("failed to save original backup", error));
        }
        let outcome = match backup.commit() {
            Ok(outcome) => outcome,
            // `commit` consumed the target; its own Drop already discarded
            // any uncommitted staged bytes, so only the error is left.
            Err(error) => {
                return Err(TransferError::io("failed to commit original backup", error));
            }
        };
        Ok(Some(outcome.relative_path))
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

/// Build the hidden backup sibling path for a destination file: a unique
/// `.tributary-backup-*` leaf in the destination's own directory.
fn backup_sibling_path(destination_relative: &Path) -> PathBuf {
    let mut backup = destination_relative
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut leaf = OsString::from(".tributary-backup-");
    leaf.push(Uuid::new_v4().to_string());
    leaf.push(".tmp");
    backup.push(leaf);
    backup
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

/// Resolve a destination that appeared between planning and staging.
///
/// Skip and Fail are represented distinctly — a Skip-policy stage is
/// skipped without committing anything, a Fail-policy stage rejects the
/// collision — while Preserve and Overwrite re-resolve naturally inside
/// prepare, so reaching this function with those policies means the
/// staging failure is a genuine I/O error.
fn post_plan_collision_outcome(
    policy: ConflictPolicy,
    planned_conflict: ConflictResolution,
    destination_relative: &Path,
    error: io::Error,
) -> Result<Option<CommittedCopy>, TransferError> {
    match (policy, planned_conflict) {
        (ConflictPolicy::Skip, ConflictResolution::Fresh) => Ok(None),
        (ConflictPolicy::Fail, ConflictResolution::Fresh) => Err(TransferError::ConflictRejected {
            path: destination_relative.to_path_buf(),
        }),
        _ => Err(TransferError::io("failed to stage destination file", error)),
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

/// Stream a backup copy with cancellation checks but no progress reports:
/// backup bytes are not transfer bytes.
fn copy_in_chunks_quiet(
    source: &mut dyn Read,
    mut staged: &std::fs::File,
    cancellation: &CancellationObserver,
) -> io::Result<()> {
    const CHUNK: usize = 64 * 1024;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if cancellation.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        staged.write_all(&buffer[..read])?;
    }
    Ok(())
}
