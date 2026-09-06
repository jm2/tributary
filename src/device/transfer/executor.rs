//! The transfer executor.
//!
//! Holds the authorities and the plan; runs the stages in order; and rolls
//! back every committed stage on any failure or cancellation. Rollback
//! removes the files this transfer actually published (by their published
//! path, not the planned one), restores originals saved aside by Overwrite
//! commits, and removes the directories this transfer created — leaving the
//! destination exactly as the transfer found it.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::types::{
    Stage, TransferError, TransferPlan, TransferProgress, TransferRequest, TransferSummary,
};
use crate::local::write_authority::{ConflictPolicy, ConflictResolution, PreparedWriteTarget};
use crate::source_lifecycle::CancellationObserver;

/// The transfer executor. Holds the authorities and the plan; runs the
/// stages in order; rolls back on failure or cancellation.
pub struct TransferExecutor {
    request: TransferRequest,
    plan: TransferPlan,
}

/// One committed copy stage, recorded so rollback undoes exactly what was
/// published: fresh and preserved publishes are removed by their actual
/// published path, and overwritten destinations have their saved original
/// restored instead of being deleted.
struct CommittedFile {
    published_relative: PathBuf,
    backup_relative: Option<PathBuf>,
}

/// What executing one stage produced.
enum StageRecord {
    /// A file was published; `published_relative` is the actual path that
    /// now names the data, and `backup_relative` the saved original moved
    /// aside by an Overwrite commit, if any.
    File {
        published_relative: PathBuf,
        backup_relative: Option<PathBuf>,
    },
    /// A directory was created by this transfer and is rollback-eligible.
    Directory(PathBuf),
    /// The destination appeared after planning and the atomic no-replace
    /// publish refused it; the staged bytes were discarded and the transfer
    /// continues without the stage.
    PostPlanSkipped,
}

/// Mutable state shared by every stage runner of one [`TransferExecutor`]
/// run: the progress sink, the cancellation observer, the running byte
/// count, and the committed work eligible for rollback.
struct RunContext<'a> {
    progress: &'a mut dyn TransferProgress,
    cancellation: &'a CancellationObserver,
    bytes_so_far: u64,
    total_bytes: u64,
    total_stages: u32,
    committed_files: Vec<CommittedFile>,
    created_directories: Vec<PathBuf>,
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
    /// Rollback restores the destination to its pre-transfer state: files
    /// the transfer published are removed, destinations the transfer
    /// overwrites have their saved originals restored, directories the
    /// transfer created are removed in reverse creation order, and saved
    /// originals are never deleted on a failed run. A rollback failure is
    /// reported instead of the triggering error, because the destination is
    /// then in a partial state the caller must know about.
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
            committed_files: Vec::new(),
            created_directories: Vec::new(),
        };
        let mut committed_stages: u32 = 0;
        let mut post_plan_skipped_stages: u32 = 0;
        for (index, stage) in self.plan.stages().iter().enumerate() {
            if context.cancellation.is_cancelled() {
                self.rollback(&mut context)?;
                return Err(TransferError::Cancelled);
            }
            context
                .progress
                .on_stage_started(stage, index as u32, context.total_stages);
            match self.run_stage(stage, index as u32, &mut context) {
                Ok(StageRecord::File {
                    published_relative,
                    backup_relative,
                }) => {
                    committed_stages = committed_stages.saturating_add(1);
                    context.committed_files.push(CommittedFile {
                        published_relative,
                        backup_relative,
                    });
                    context.progress.on_stage_completed(
                        stage,
                        index as u32,
                        context.total_stages,
                        context.bytes_so_far,
                        context.total_bytes,
                    );
                }
                Ok(StageRecord::Directory(relative)) => {
                    committed_stages = committed_stages.saturating_add(1);
                    context.created_directories.push(relative);
                    context.progress.on_stage_completed(
                        stage,
                        index as u32,
                        context.total_stages,
                        context.bytes_so_far,
                        context.total_bytes,
                    );
                }
                Ok(StageRecord::PostPlanSkipped) => {
                    post_plan_skipped_stages = post_plan_skipped_stages.saturating_add(1);
                    context
                        .progress
                        .on_stage_post_plan_skipped(stage, index as u32, context.total_stages);
                }
                Err(error) => {
                    self.rollback(&mut context)?;
                    return Err(error);
                }
            }
        }
        // Every stage committed: the saved originals the Overwrite commits
        // left behind are no longer needed. A backup that cannot be removed
        // fails the transfer and restores everything rather than littering
        // the mount with hidden originals of uncertain state.
        if let Err(error) = self.discard_backups(&mut context) {
            self.rollback(&mut context)?;
            return Err(error);
        }
        Ok(TransferSummary {
            committed_stages,
            post_plan_skipped_stages,
            bytes_copied: context.bytes_so_far,
            completed: true,
        })
    }

    /// Execute one stage.
    fn run_stage(
        &self,
        stage: &Stage,
        stage_index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<StageRecord, TransferError> {
        match stage {
            Stage::CreateDirectory {
                destination_relative_path,
            } => {
                self.execute_create_directory(destination_relative_path, context)?;
                Ok(StageRecord::Directory(destination_relative_path.clone()))
            }
            Stage::CopyFile {
                source_relative_path,
                destination_relative_path,
                bytes,
                conflict,
                ..
            } => self.execute_copy_file(
                source_relative_path,
                destination_relative_path,
                *bytes,
                *conflict,
                stage_index,
                context,
            ),
            Stage::RemoveFile { .. } => {
                // RemoveFile stages are inserted only by the rollback path
                // and never appear in a forward plan. Skip defensively.
                Ok(StageRecord::PostPlanSkipped)
            }
        }
    }

    /// Create one destination directory. Idempotent: an existing directory
    /// with the same identity is not an error. Only directories this run
    /// actually created are recorded for rollback; a pre-existing directory
    /// is never removed on failure.
    fn execute_create_directory(
        &self,
        relative: &Path,
        context: &mut RunContext<'_>,
    ) -> Result<(), TransferError> {
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;
        let final_path = self.request.destination.root().join(relative);
        let pre_existing = std::fs::symlink_metadata(&final_path).is_ok();
        // Directory stages carry no file conflict decision: the transfer
        // creates the missing directory tree, accepting directories that
        // already exist regardless of the file conflict policy.
        match self
            .request
            .destination
            .create_relative_directory(relative, ConflictPolicy::Preserve)
        {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A concurrent writer created it between the existence probe
                // and the create: treat it as pre-existing when it is now a
                // directory, refuse otherwise.
                match std::fs::symlink_metadata(&final_path) {
                    Ok(metadata) if metadata.is_dir() => {}
                    _ => {
                        return Err(TransferError::io(
                            "directory creation failed with AlreadyExists",
                            error,
                        ));
                    }
                }
            }
            Err(error) => return Err(TransferError::io("create directory failed", error)),
        }
        if !pre_existing {
            context.created_directories.push(relative.to_path_buf());
        }
        Ok(())
    }

    /// Validate both authorities, then copy one source file into a staged
    /// destination file and commit it. The planned conflict resolution is
    /// consumed verbatim: the executor never re-decides a conflict.
    fn execute_copy_file(
        &self,
        source_relative: &Path,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<StageRecord, TransferError> {
        self.request
            .source
            .validate()
            .map_err(|error| TransferError::authority(format!("source not current: {error}")))?;
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;
        // The read authority closure is io-typed; the transfer error travels
        // through this side channel so the exact error (including
        // cancellation) reaches the caller unflattened.
        let mut transfer_error: Option<TransferError> = None;
        let staged_result = self
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
                    Ok(record) => Ok(record),
                    Err(error) => {
                        transfer_error = Some(error);
                        Err(io::Error::other("transfer stage failed"))
                    }
                }
            });
        if let Some(error) = transfer_error.take() {
            return Err(error);
        }
        staged_result.map_err(|error| {
            TransferError::io("failed to open source file for copy", error)
        })
    }

    /// Stage one destination file, stream the source into it through the
    /// authority-validating staged writer, and commit it through the planned
    /// resolution.
    ///
    /// A cancelled or short copy rolls the staged file back and repays the
    /// reported byte count. A no-replace publish refused by a destination
    /// that appeared after planning is a distinct post-plan skip; every other
    /// commit failure is an error and triggers full rollback.
    fn stage_and_commit_file(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        planned_conflict: ConflictResolution,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut dyn Read,
    ) -> Result<StageRecord, TransferError> {
        let mut staged = self
            .request
            .destination
            .prepare_write_with_resolution(destination_relative, planned_conflict)
            .map_err(|error| TransferError::io("failed to stage destination file", error))?;
        let copied = match copy_in_chunks(source_file, &mut staged, stage_index, context) {
            Ok(copied) => copied,
            Err((copied, mut error)) => {
                context.bytes_so_far = context.bytes_so_far.saturating_sub(copied);
                let rollback_result = staged.rollback();
                if matches!(error, TransferError::Cancelled) {
                    return Err(TransferError::Cancelled);
                }
                if let Err(rollback_error) = rollback_result {
                    error = TransferError::io(
                        "staged cleanup after copy failure also failed",
                        rollback_error,
                    );
                }
                return Err(error);
            }
        };
        if declared_bytes != 0 && copied != declared_bytes {
            context.bytes_so_far = context.bytes_so_far.saturating_sub(copied);
            let _ = staged.rollback();
            return Err(TransferError::io(
                "staged copy does not match the planned source size",
                io::Error::other(format!(
                    "source produced {copied} bytes but the plan declared {declared_bytes}"
                )),
            ));
        }
        match staged.commit() {
            Ok(outcome) => Ok(StageRecord::File {
                published_relative: outcome.relative_path,
                backup_relative: outcome.backup_relative_path,
            }),
            Err(error) => {
                context.bytes_so_far = context.bytes_so_far.saturating_sub(copied);
                if error.kind() == io::ErrorKind::AlreadyExists
                    && planned_conflict != ConflictResolution::Overwrite
                {
                    // The planned Fresh/Preserved publish is atomic
                    // no-replace: a destination that appeared after planning
                    // refuses the replace instead of being clobbered. Report
                    // the skip distinctly; the staged file is discarded by
                    // the target's Drop.
                    return Ok(StageRecord::PostPlanSkipped);
                }
                Err(TransferError::io("staged commit failed", error))
            }
        }
    }

    /// Undo committed work in reverse order: restore overwritten originals,
    /// remove published fresh/preserved files, then remove the directories
    /// this run created. The destination authority is revalidated before
    /// every operation; an authority lost mid-rollback is reported rather
    /// than silently skipping the remaining cleanup.
    fn rollback(&self, context: &mut RunContext<'_>) -> Result<(), TransferError> {
        while let Some(committed) = context.committed_files.pop() {
            self.request.destination.validate().map_err(|error| {
                TransferError::authority(format!("destination not current during rollback: {error}"))
            })?;
            let outcome = match committed.backup_relative {
                Some(backup) => self
                    .request
                    .destination
                    .restore_overwritten_file(&committed.published_relative, &backup),
                None => self
                    .request
                    .destination
                    .remove_relative_file(&committed.published_relative),
            };
            outcome.map_err(|error| TransferError::RollbackFailed {
                path: committed.published_relative.clone(),
                context: error.to_string(),
            })?;
        }
        while let Some(relative) = context.created_directories.pop() {
            self.request.destination.validate().map_err(|error| {
                TransferError::authority(format!("destination not current during rollback: {error}"))
            })?;
            match self.request.destination.remove_relative_directory(&relative) {
                Ok(()) => {}
                // Already gone: nothing to undo.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // No longer empty: entries this transfer did not create live
                // here. Leave the directory rather than deleting foreign
                // data; every entry the transfer created was already undone
                // above.
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => {
                    return Err(TransferError::RollbackFailed {
                        path: relative,
                        context: error.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Remove the saved originals left behind by successful Overwrite
    /// commits once the whole transfer has committed. Each removed backup is
    /// cleared from the rollback records so a later failure removes the
    /// published file instead of attempting a restore whose original is
    /// already gone.
    fn discard_backups(&self, context: &mut RunContext<'_>) -> Result<(), TransferError> {
        for index in 0..context.committed_files.len() {
            let Some(backup) = context.committed_files[index].backup_relative.clone() else {
                continue;
            };
            self.request.destination.validate().map_err(|error| {
                TransferError::authority(format!("destination not current: {error}"))
            })?;
            self.request
                .destination
                .remove_relative_file(&backup)
                .map_err(|error| {
                    TransferError::io(
                        format!("failed to remove overwrite backup {backup:?}"),
                        error,
                    )
                })?;
            context.committed_files[index].backup_relative = None;
        }
        Ok(())
    }
}

/// Stream `source` into the staged file in fixed-size chunks through the
/// staged target's authority-validating writer, reporting progress after
/// every chunk and checking cancellation between chunks. Returns the byte
/// count, or the count reached together with the failure so the caller can
/// repay the reported bytes.
fn copy_in_chunks(
    source: &mut dyn Read,
    staged: &mut PreparedWriteTarget,
    stage_index: u32,
    context: &mut RunContext<'_>,
) -> Result<u64, (u64, TransferError)> {
    const CHUNK: usize = 64 * 1024;
    let mut buffer = vec![0u8; CHUNK];
    let mut copied: u64 = 0;
    loop {
        if context.cancellation.is_cancelled() {
            return Err((copied, TransferError::Cancelled));
        }
        let read = source
            .read(&mut buffer)
            .map_err(|error| (copied, TransferError::io("failed to read source file", error)))?;
        if read == 0 {
            break;
        }
        staged.write_all(&buffer[..read]).map_err(|error| {
            (
                copied,
                TransferError::io("failed to write staged file", error),
            )
        })?;
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
