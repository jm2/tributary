//! Public value types shared by the transfer planner and executor.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use crate::local::root_authority::MountedRootAuthority;
use crate::local::write_authority::{ConflictPolicy, ConflictResolution, MountedWriteAuthority};

/// One source-destination pair to transfer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferItem {
    /// Source path relative to the source authority's root.
    pub source_relative_path: PathBuf,
    /// Destination path relative to the destination authority's root.
    pub destination_relative_path: PathBuf,
}

impl TransferItem {
    /// Convenience constructor for a same-relative-path transfer.
    pub fn same(relative: PathBuf) -> Self {
        Self {
            source_relative_path: relative.clone(),
            destination_relative_path: relative,
        }
    }

    /// Construct a transfer where the source and destination differ.
    pub fn new(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source_relative_path: source,
            destination_relative_path: destination,
        }
    }
}

/// What a single stage of a transfer plan actually does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Stage {
    /// Create the directory (and any missing ancestors) at this destination
    /// path. The stage is idempotent: an already-present directory with the
    /// same identity does not error.
    CreateDirectory {
        /// Destination path relative to the destination authority's root.
        destination_relative_path: PathBuf,
    },
    /// Copy a regular file from the source to the destination.
    CopyFile {
        /// Source path relative to the source authority's root.
        source_relative_path: PathBuf,
        /// Destination path relative to the destination authority's root.
        destination_relative_path: PathBuf,
        /// Declared byte size of the source file.
        bytes: u64,
        /// True when the staged file is committed by an atomic rename on the
        /// destination filesystem; false when the planner fell back to a
        /// non-atomic path (cross-filesystem, or source authority absent).
        atomic: bool,
        /// How the conflict policy was resolved before staging.
        conflict: ConflictResolution,
    },
    /// Remove a previously published destination file. Used for rollback.
    RemoveFile {
        /// Destination path relative to the destination authority's root.
        destination_relative_path: PathBuf,
    },
}

impl Stage {
    /// Human-readable stage type label, for logging and progress.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CreateDirectory { .. } => "create-directory",
            Self::CopyFile { .. } => "copy-file",
            Self::RemoveFile { .. } => "remove-file",
        }
    }
}

/// A fully resolved transfer plan ready to execute.
#[derive(Clone, Debug)]
pub struct TransferPlan {
    pub(super) stages: Vec<Stage>,
    pub(super) total_bytes: u64,
    pub(super) file_count: u32,
    pub(super) directory_count: u32,
}

impl TransferPlan {
    /// All stages in execution order.
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Total bytes the executor will copy. Used for capacity budgeting and
    /// progress reporting.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Number of file copy stages in the plan.
    pub fn file_count(&self) -> u32 {
        self.file_count
    }

    /// Number of directory creation stages in the plan.
    pub fn directory_count(&self) -> u32 {
        self.directory_count
    }

    /// Sum of file and directory stages.
    pub fn stage_count(&self) -> u32 {
        self.file_count + self.directory_count
    }

    /// True when the plan has no work to do.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

/// Errors produced while planning or executing a transfer.
#[derive(Debug, Error)]
pub enum TransferError {
    /// A relative path was absolute, empty, or contained a non-normal
    /// component.
    #[error("transfer item path is invalid: {path:?}")]
    InvalidItemPath {
        /// The offending path.
        path: PathBuf,
    },
    /// A source entry could not be read or its type was unsupported.
    #[error("source entry {path:?} is not a regular file or directory")]
    UnsupportedSourceEntry {
        /// The offending path.
        path: PathBuf,
    },
    /// The destination's capacity budget would be exceeded by the plan.
    #[error("transfer plan requires {required} bytes but capacity budget is {budget} bytes")]
    CapacityExceeded {
        /// Bytes the plan requires.
        required: u64,
        /// Bytes the caller allows.
        budget: u64,
    },
    /// A conflict policy rejected the operation because the destination
    /// already exists.
    #[error("destination {path:?} already exists and policy forbids it")]
    ConflictRejected {
        /// The offending destination path.
        path: PathBuf,
    },
    /// The source or destination authority is no longer current.
    #[error("authority is no longer current: {context}")]
    AuthorityLost {
        /// Human-readable context for the loss.
        context: String,
    },
    /// Caller-supplied cancellation fired.
    #[error("transfer was cancelled")]
    Cancelled,
    /// A staged file failed to commit.
    #[error("staged file failed to commit: {context}")]
    CommitFailed {
        /// Human-readable context for the failure.
        context: String,
    },
    /// A rollback stage itself failed.
    #[error("rollback failed at {path:?}: {context}")]
    RollbackFailed {
        /// The path whose rollback failed.
        path: PathBuf,
        /// Human-readable context for the failure.
        context: String,
    },
    /// Underlying I/O error.
    #[error("transfer I/O error: {context}")]
    Io {
        /// Human-readable context for the error.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

impl TransferError {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn authority(context: impl Into<String>) -> Self {
        Self::AuthorityLost {
            context: context.into(),
        }
    }
}

/// What the planner was told up front.
pub struct TransferRequest {
    /// Read authority for the source mount. Held by `Arc` so the executor can
    /// reuse the same authority throughout a single transfer.
    pub source: Arc<MountedRootAuthority>,
    /// Write authority for the destination mount. Cloned cheaply.
    pub destination: MountedWriteAuthority,
    /// Ordered list of source-destination pairs. Order is preserved so callers
    /// can express playlist-order or directory-recursion intent.
    pub items: Vec<TransferItem>,
    /// How to handle a destination that already exists.
    pub conflict_policy: ConflictPolicy,
    /// Optional byte budget; the plan is rejected when its total bytes
    /// exceed the budget. `None` means no budget.
    pub capacity_budget: Option<u64>,
    /// Whether directory items should be expanded recursively. When `true`
    /// (the default), a directory item transfers every contained regular
    /// file; when `false`, only the directory itself is created.
    pub recurse_directories: bool,
}

impl TransferRequest {
    /// Construct a minimal request: every item uses the same relative path,
    /// default conflict policy, no budget, and recursive directory walk.
    pub fn simple(
        source: Arc<MountedRootAuthority>,
        destination: MountedWriteAuthority,
        items: Vec<TransferItem>,
    ) -> Self {
        Self {
            source,
            destination,
            items,
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        }
    }
}

/// Per-stage progress callback. The callback may be invoked from any thread;
/// the executor never holds the callback across an `await` boundary.
pub trait TransferProgress: Send {
    /// Called when the executor starts a stage.
    fn on_stage_started(&mut self, _stage: &Stage, _index: u32, _total: u32) {}
    /// Called when the executor completes a stage.
    fn on_stage_completed(
        &mut self,
        _stage: &Stage,
        _index: u32,
        _total: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
    }
    /// Called while a file copy is in progress, at most once per buffer
    /// chunk. Implementations should remain cheap; the executor flushes
    /// between calls.
    fn on_bytes_copied(
        &mut self,
        _stage_index: u32,
        _total_stages: u32,
        _bytes_so_far: u64,
        _total_bytes: u64,
    ) {
    }
}

/// No-op progress sink used when the caller does not supply one.
impl TransferProgress for () {}

/// What the executor produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransferSummary {
    /// Number of stages that were fully committed.
    pub committed_stages: u32,
    /// Total bytes successfully copied to the destination.
    pub bytes_copied: u64,
    /// Set to `true` when the executor completed every stage in the plan.
    pub completed: bool,
}
