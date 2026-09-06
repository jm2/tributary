//! Transfer execution: run a planned stage list with rollback.
//!
//! The executor holds the request authorities and the plan, walks the
//! stages in order, reports progress, observes cancellation between
//! stages and at every copy chunk, and rolls back already-committed
//! files on any failure or cancellation.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::{Stage, TransferError, TransferPlan, TransferProgress, TransferRequest, TransferSummary};
use crate::local::write_authority::{
    CommitOutcome, ConflictPolicy, MountedWriteAuthority, PreparedWriteTarget,
};
use crate::source_lifecycle::CancellationObserver;

/// Progress and cancellation context for one copy stage.
///
/// Groups the progress-related parameters of
/// [`TransferExecutor::execute_copy_file`] so the copy loop reads
/// through a single borrowed context instead of a long parameter list.
struct CopyStageContext<'a> {
    /// Running byte total shared across all stages of the plan.
    bytes_so_far: &'a mut u64,
    total_bytes: u64,
    stage_index: u32,
    total_stages: u32,
    progress: &'a mut dyn TransferProgress,
    cancellation: &'a CancellationObserver,
}

/// The transfer executor. Holds the authorities and the plan; runs the
/// stages in order; rolls back on failure or cancellation.
pub struct TransferExecutor {
    request: TransferRequest,
    plan: TransferPlan,
}

impl TransferExecutor {
    /// Construct an executor from a previously planned request.
    pub fn new(request: TransferRequest, plan: TransferPlan) -> Self {
        Self { request, plan }
    }

    /// Run the plan to completion, reporting progress through `progress`,
    /// observing `cancellation` between stages, and rolling back committed
    /// stages on any error.
    pub fn run(
        self,
        progress: &mut dyn TransferProgress,
        cancellation: &CancellationObserver,
    ) -> Result<TransferSummary, TransferError> {
        let total_stages = self.plan.stage_count();
        let total_bytes = self.plan.total_bytes();
        let mut committed_stages: u32 = 0;
        let mut bytes_so_far: u64 = 0;
        let mut committed_files: Vec<PathBuf> = Vec::new();

        for (index, stage) in self.plan.stages().iter().enumerate() {
            if cancellation.is_cancelled() {
                rollback_committed(&self.request.destination, &mut committed_files)?;
                return Err(TransferError::Cancelled);
            }
            progress.on_stage_started(stage, index as u32, total_stages);
            let mut context = CopyStageContext {
                bytes_so_far: &mut bytes_so_far,
                total_bytes,
                stage_index: index as u32,
                total_stages,
                progress,
                cancellation,
            };
            match self.execute_stage(stage, &mut context) {
                Ok(Some(published)) => committed_files.push(published),
                Ok(None) => {}
                Err(error) => {
                    // Every stage failure — including a mid-copy
                    // cancellation — rolls back already-committed files
                    // before propagating, as promised by the module-level
                    // rollback contract.
                    rollback_committed(&self.request.destination, &mut committed_files)?;
                    return Err(error);
                }
            }
            committed_stages = committed_stages.saturating_add(1);
            progress.on_stage_completed(
                stage,
                index as u32,
                total_stages,
                bytes_so_far,
                total_bytes,
            );
        }

        Ok(TransferSummary {
            committed_stages,
            bytes_copied: bytes_so_far,
            completed: true,
        })
    }

    /// Execute one stage. `Ok(Some(path))` reports the relative path that
    /// was actually published, which under `Preserve` is a disambiguated
    /// sibling rather than the planned destination, so rollback targets
    /// the real file.
    fn execute_stage(
        &self,
        stage: &Stage,
        context: &mut CopyStageContext<'_>,
    ) -> Result<Option<PathBuf>, TransferError> {
        match stage {
            Stage::CreateDirectory {
                destination_relative_path,
            } => {
                self.execute_create_directory(destination_relative_path)?;
                Ok(None)
            }
            Stage::CopyFile {
                source_relative_path,
                destination_relative_path,
                bytes,
                atomic: _,
                conflict: _,
            } => {
                let outcome = self.execute_copy_file(
                    source_relative_path,
                    destination_relative_path,
                    *bytes,
                    context,
                )?;
                Ok(Some(outcome.relative_path))
            }
            Stage::RemoveFile { .. } => {
                // RemoveFile stages are inserted only by the rollback path
                // and never appear in a forward plan. Skip defensively.
                Ok(None)
            }
        }
    }

    fn execute_create_directory(&self, relative: &Path) -> Result<(), TransferError> {
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;
        match self
            .request
            .destination
            .create_relative_directory(relative, self.request.conflict_policy)
        {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // Idempotent: an existing directory is not an error.
                let final_path = self.request.destination.root().join(relative);
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

    fn execute_copy_file(
        &self,
        source_relative: &Path,
        destination_relative: &Path,
        declared_bytes: u64,
        context: &mut CopyStageContext<'_>,
    ) -> Result<CommitOutcome, TransferError> {
        self.request
            .source
            .validate()
            .map_err(|error| TransferError::authority(format!("source not current: {error}")))?;
        self.request
            .destination
            .validate()
            .map_err(|error| {
                TransferError::authority(format!("destination not current: {error}"))
            })?;

        let destination = &self.request.destination;
        let policy = self.request.conflict_policy;
        let source_result = self.request.source.with_relative_file(
            source_relative,
            |mut source_file| -> io::Result<(CommitOutcome, u64)> {
                stage_and_copy(
                    destination,
                    destination_relative,
                    policy,
                    declared_bytes,
                    context,
                    &mut source_file,
                )
            },
        );
        let (outcome, _copied) = match source_result {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Err(TransferError::Cancelled);
            }
            Err(error) => {
                return Err(TransferError::io("failed to copy source file", error));
            }
        };
        Ok(outcome)
    }
}

/// Stage a destination file and copy the source into it, then commit.
///
/// The staged file is the unit of work: a mid-copy cancellation or a
/// size mismatch rolls the staged file back before the error is
/// surfaced, leaving the destination untouched.
fn stage_and_copy(
    destination: &MountedWriteAuthority,
    destination_relative: &Path,
    policy: ConflictPolicy,
    declared_bytes: u64,
    context: &mut CopyStageContext<'_>,
    source_file: &mut std::fs::File,
) -> io::Result<(CommitOutcome, u64)> {
    let mut staged = destination
        .prepare_write_relative_file(destination_relative, policy)
        .map_err(|error| TransferError::io("failed to stage destination file", error))
        .map_err(io::Error::other)?;

    let copied = copy_chunks(&mut staged, source_file, context)?;

    if declared_bytes != 0 && copied != declared_bytes {
        let _ = staged.rollback();
        return Err(io::Error::other(format!(
            "source size {copied} differs from declared {declared_bytes} bytes"
        )));
    }

    let outcome = staged
        .commit()
        .map_err(|error| TransferError::io("staged commit failed", error))
        .map_err(io::Error::other)?;
    Ok((outcome, copied))
}

/// Buffered chunked copy with per-chunk cancellation checks, progress
/// reporting, and authority revalidation.
///
/// Chunks are written through [`PreparedWriteTarget::write_all`], not
/// the raw staged handle: the target revalidates the destination
/// authority around every chunk (and flushes), so an unmount mid-copy
/// fails closed instead of writing past an invalid boundary.
fn copy_chunks(
    staged: &mut PreparedWriteTarget,
    source_file: &mut std::fs::File,
    context: &mut CopyStageContext<'_>,
) -> io::Result<u64> {
    const CHUNK: usize = 64 * 1024;
    let mut buffer = vec![0u8; CHUNK];
    let mut copied: u64 = 0;
    loop {
        if context.cancellation.is_cancelled() {
            let _ = staged.rollback();
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let read = source_file
            .read(&mut buffer)
            .map_err(|error| TransferError::io("failed to read source file", error))
            .map_err(io::Error::other)?;
        if read == 0 {
            break;
        }
        staged
            .write_all(&buffer[..read])
            .map_err(|error| TransferError::io("failed to write staged file", error))
            .map_err(io::Error::other)?;
        copied = copied.saturating_add(read as u64);
        *context.bytes_so_far = context.bytes_so_far.saturating_add(read as u64);
        context.progress.on_bytes_copied(
            context.stage_index,
            context.total_stages,
            *context.bytes_so_far,
            context.total_bytes,
        );
    }
    Ok(copied)
}

/// Roll back committed files in reverse order, revalidating the
/// destination authority before each removal so a remount between
/// commit and rollback cannot authorise removal of a replacement file
/// that occupies the old path.
fn rollback_committed(
    destination: &MountedWriteAuthority,
    committed_files: &mut Vec<PathBuf>,
) -> Result<(), TransferError> {
    rollback_inner(destination, committed_files).map_err(|error| {
        TransferError::RollbackFailed {
            path: PathBuf::new(),
            context: error.to_string(),
        }
    })
}

fn rollback_inner(
    destination: &MountedWriteAuthority,
    committed_files: &mut Vec<PathBuf>,
) -> io::Result<()> {
    while let Some(relative) = committed_files.pop() {
        destination
            .validate()
            .map_err(|error| {
                TransferError::authority(format!("destination not current: {error}"))
            })
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
        destination.remove_relative_file(&relative)?;
    }
    Ok(())
}
