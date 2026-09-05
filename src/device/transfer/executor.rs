//! The transfer executor.
//!
//! Holds the authorities and the plan; runs the stages in order; rolls back
//! committed stages on failure or cancellation.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::types::{
    Stage, TransferError, TransferPlan, TransferProgress, TransferRequest, TransferSummary,
};
use crate::local::write_authority::PreparedWriteTarget;
use crate::source_lifecycle::CancellationObserver;

/// The transfer executor. Holds the authorities and the plan; runs the
/// stages in order; rolls back on failure or cancellation.
pub struct TransferExecutor {
    request: TransferRequest,
    plan: TransferPlan,
}

/// Mutable state shared by every stage runner of one [`TransferExecutor`]
/// run: the progress sink, the cancellation observer, the running byte
/// count, and the committed files eligible for rollback.
struct RunContext<'a> {
    progress: &'a mut dyn TransferProgress,
    cancellation: &'a CancellationObserver,
    bytes_so_far: u64,
    total_bytes: u64,
    total_stages: u32,
    committed_files: Vec<PathBuf>,
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
        let mut context = RunContext {
            progress,
            cancellation,
            bytes_so_far: 0,
            total_bytes: self.plan.total_bytes(),
            total_stages: self.plan.stage_count(),
            committed_files: Vec::new(),
        };
        let mut committed_stages: u32 = 0;
        for (index, stage) in self.plan.stages().iter().enumerate() {
            if context.cancellation.is_cancelled() {
                self.rollback(&mut context)?;
                return Err(TransferError::Cancelled);
            }
            context
                .progress
                .on_stage_started(stage, index as u32, context.total_stages);
            self.run_stage(stage, index as u32, &mut context)?;
            committed_stages = committed_stages.saturating_add(1);
            context.progress.on_stage_completed(
                stage,
                index as u32,
                context.total_stages,
                context.bytes_so_far,
                context.total_bytes,
            );
        }
        Ok(TransferSummary {
            committed_stages,
            bytes_copied: context.bytes_so_far,
            completed: true,
        })
    }

    /// Execute one stage, recording committed file copies for rollback.
    fn run_stage(
        &self,
        stage: &Stage,
        index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<(), TransferError> {
        match stage {
            Stage::CreateDirectory {
                destination_relative_path,
            } => self.execute_create_directory(destination_relative_path),
            Stage::CopyFile {
                source_relative_path,
                destination_relative_path,
                bytes,
                ..
            } => {
                self.execute_copy_file(
                    source_relative_path,
                    destination_relative_path,
                    *bytes,
                    index,
                    context,
                )?;
                context
                    .committed_files
                    .push(destination_relative_path.clone());
                Ok(())
            }
            Stage::RemoveFile { .. } => {
                // RemoveFile stages are inserted only by the rollback path
                // and never appear in a forward plan. Skip defensively.
                Ok(())
            }
        }
    }

    /// Create one destination directory. Idempotent: an existing directory
    /// with the same identity is not an error.
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

    /// Validate both authorities, then copy one source file into a staged
    /// destination file and commit it atomically.
    fn execute_copy_file(
        &self,
        source_relative: &Path,
        destination_relative: &Path,
        declared_bytes: u64,
        stage_index: u32,
        context: &mut RunContext<'_>,
    ) -> Result<(), TransferError> {
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
                self.stage_and_commit_file(
                    destination_relative,
                    declared_bytes,
                    stage_index,
                    context,
                    &mut source_file,
                )
            });
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                Err(TransferError::Cancelled)
            }
            Err(error) => Err(TransferError::io("failed to copy source file", error)),
        }
    }

    /// Stage one destination file, stream the source into it, and commit.
    /// A cancelled or short copy rolls the staged file back; a flush or
    /// commit failure is cleaned up by the staged target's `Drop`.
    fn stage_and_commit_file(
        &self,
        destination_relative: &Path,
        declared_bytes: u64,
        stage_index: u32,
        context: &mut RunContext<'_>,
        source_file: &mut dyn Read,
    ) -> io::Result<()> {
        let staged = self
            .request
            .destination
            .prepare_write_relative_file(destination_relative, self.request.conflict_policy)
            .map_err(|error| TransferError::io("failed to stage destination file", error))
            .map_err(io::Error::other)?;
        let copied = match copy_in_chunks(source_file, &staged, stage_index, context) {
            Ok(copied) => copied,
            Err(error) => {
                let _ = staged.rollback();
                return Err(error);
            }
        };
        staged
            .staged_file()
            .flush()
            .map_err(|error| TransferError::io("failed to flush staged file", error))
            .map_err(io::Error::other)?;
        if declared_bytes != 0 && copied != declared_bytes {
            let _ = staged.rollback();
            return Err(io::Error::other(format!(
                "source size {copied} differs from declared {declared_bytes} bytes"
            )));
        }
        staged
            .commit()
            .map_err(|error| TransferError::io("staged commit failed", error))
            .map_err(io::Error::other)?;
        Ok(())
    }

    /// Remove committed files in reverse commit order, revalidating the
    /// destination authority before each removal.
    fn rollback(&self, context: &mut RunContext<'_>) -> Result<(), TransferError> {
        while let Some(relative) = context.committed_files.pop() {
            self.request.destination.validate().map_err(|error| {
                TransferError::authority(format!("destination not current: {error}"))
            })?;
            self.request
                .destination
                .remove_relative_file(&relative)
                .map_err(|error| TransferError::RollbackFailed {
                    path: relative,
                    context: error.to_string(),
                })?;
        }
        Ok(())
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
