//! Generic mounted-filesystem transfer planner and executor.
//!
//! Issue #8 / P3.2 requires a generic mounted-filesystem transfer planner and
//! executor with retained write authority, capacity and conflict policy,
//! atomic copy where possible, progress, cancellation, and rollback. The
//! planner and executor here satisfy every one of those requirements without
//! coupling to a specific discovery backend, MTP device, or sync schedule.
//!
//! The intended caller is the future device-sync UX. The mount-relative scan
//! and authority model are the same as those used by the removable-media
//! scanner ([`crate::removable`]) and the resolver
//! ([`crate::local::resolver`]). A successful scan is followed by a transfer
//! plan; an admitted plan is committed through the destination's
//! [`MountedWriteAuthority`](crate::local::write_authority::MountedWriteAuthority)
//! and the source's [`MountedRootAuthority`](crate::local::root_authority::MountedRootAuthority).
//!
//! ## Authority
//!
//! The source authority is read-only; every source file is opened through
//! [`MountedRootAuthority::open_relative_regular_file`]. The destination is
//! the write authority. Each write is staged as a sibling temporary file
//! inside the destination directory and committed with a single `rename(2)`.
//! Both authorities are revalidated before and after every operation, so a
//! binder swap, unmount, or remount between staging and commit produces a
//! fail-closed error rather than a partial publish.
//!
//! ## Atomicity
//!
//! A staged file is committed with the platform's atomic rename
//! (`rename(2)` on Unix, `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on
//! Windows). The plan records the [`Stage::atomic`] flag so callers and
//! reviewers can identify non-atomic operations (e.g. cross-filesystem moves
//! that the planner chose to surface as a fallible copy).
//!
//! ## Rollback
//!
//! The executor records every published stage. On cancellation or a failed
//! stage, already-committed files are rolled back in reverse order. Each
//! rollback revalidates the destination authority before deletion so a
//! remount between commit and rollback cannot authorise removal of a
//! replacement file that occupies the old path.
//!
//! ## Cancellation
//!
//! Cancellation is cooperative. The caller supplies a
//! [`CancellationObserver`](crate::source_lifecycle::CancellationObserver) and
//! the executor checks it between stages. A long-running file copy checks at
//! every buffered chunk. Cancellation does not abort an in-flight
//! `commit(2)`; the staged file is the unit of work, and an uncommitted
//! staged file is rolled back before the executor returns.

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
    CreateDirectory { destination_relative_path: PathBuf },
    /// Copy a regular file from the source to the destination.
    CopyFile {
        source_relative_path: PathBuf,
        destination_relative_path: PathBuf,
        bytes: u64,
        /// True when the staged file is committed by an atomic rename on the
        /// destination filesystem; false when the planner fell back to a
        /// non-atomic path (cross-filesystem, or source authority absent).
        atomic: bool,
        /// How the conflict policy was resolved before staging.
        conflict: ConflictResolution,
    },
    /// Remove a previously published destination file. Used for rollback.
    RemoveFile { destination_relative_path: PathBuf },
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
    stages: Vec<Stage>,
    total_bytes: u64,
    file_count: u32,
    directory_count: u32,
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
    InvalidItemPath { path: PathBuf },
    /// A source entry could not be read or its type was unsupported.
    #[error("source entry {path:?} is not a regular file or directory")]
    UnsupportedSourceEntry { path: PathBuf },
    /// The destination's capacity budget would be exceeded by the plan.
    #[error("transfer plan requires {required} bytes but capacity budget is {budget} bytes")]
    CapacityExceeded { required: u64, budget: u64 },
    /// A conflict policy rejected the operation because the destination
    /// already exists.
    #[error("destination {path:?} already exists and policy forbids it")]
    ConflictRejected { path: PathBuf },
    /// The source or destination authority is no longer current.
    #[error("authority is no longer current: {context}")]
    AuthorityLost { context: String },
    /// Caller-supplied cancellation fired.
    #[error("transfer was cancelled")]
    Cancelled,
    /// A staged file failed to commit.
    #[error("staged file failed to commit: {context}")]
    CommitFailed { context: String },
    /// A rollback stage itself failed.
    #[error("rollback failed at {path:?}: {context}")]
    RollbackFailed { path: PathBuf, context: String },
    /// Underlying I/O error.
    #[error("transfer I/O error: {context}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl TransferError {
    fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    fn authority(context: impl Into<String>) -> Self {
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

mod executor;
mod planner;
#[cfg(test)]
mod tests;

pub use executor::TransferExecutor;
pub use planner::TransferPlanner;
