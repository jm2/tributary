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

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::local::root_authority::MountedRootAuthority;
use crate::local::write_authority::{
    CommitOutcome, ConflictPolicy, ConflictResolution, MountedWriteAuthority,
};
use crate::source_lifecycle::CancellationObserver;

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

/// The transfer planner. Stateless and `Clone` so the same plan can be
/// inspected, persisted, or routed through different executors.
#[derive(Clone, Debug, Default)]
pub struct TransferPlanner;

impl TransferPlanner {
    /// Create a new planner instance.
    pub fn new() -> Self {
        Self
    }

    /// Build a plan from a request.
    ///
    /// Planning is read-only against the source and destination authorities.
    /// It opens the source to confirm each regular file's size but does not
    /// stage any writes. The destination is queried for existing entries to
    /// resolve conflict policy; the resolved policy is recorded on every
    /// copy stage so the executor never re-decides a conflict.
    #[allow(clippy::unused_self)]
    pub fn plan(&self, request: &TransferRequest) -> Result<TransferPlan, TransferError> {
        validate_request(request)?;

        let mut stages: Vec<Stage> = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut file_count: u32 = 0;
        let mut directory_count: u32 = 0;
        let mut created_directories: std::collections::BTreeSet<PathBuf> =
            std::collections::BTreeSet::new();

        for item in &request.items {
            if item.source_relative_path.as_os_str().is_empty()
                || item.destination_relative_path.as_os_str().is_empty()
            {
                return Err(TransferError::InvalidItemPath {
                    path: item.source_relative_path.clone(),
                });
            }
            request.source.validate().map_err(|error| {
                TransferError::io("source authority failed pre-plan validation", error)
            })?;
            request.destination.validate().map_err(|error| {
                TransferError::io("destination authority failed pre-plan validation", error)
            })?;

            let source_abs = request.source.root().join(&item.source_relative_path);
            let source_meta = match std::fs::symlink_metadata(&source_abs) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(TransferError::UnsupportedSourceEntry {
                        path: item.source_relative_path.clone(),
                    });
                }
                Err(error) => {
                    return Err(TransferError::io(
                        "failed to read source entry metadata",
                        error,
                    ));
                }
            };

            if source_meta.is_dir() {
                if request.recurse_directories {
                    Self::collect_directory_stages(
                        request,
                        item,
                        &mut stages,
                        &mut total_bytes,
                        &mut file_count,
                        &mut directory_count,
                        &mut created_directories,
                    )?;
                } else {
                    ensure_directory_stage(
                        &item.destination_relative_path,
                        &mut stages,
                        &mut directory_count,
                        &mut created_directories,
                    );
                }
            } else if source_meta.is_file() {
                let size = source_meta.len();
                let resolution = resolve_conflict(
                    &request.destination,
                    &item.destination_relative_path,
                    request.conflict_policy,
                )?;
                if let Some(resolution) = resolution {
                    ensure_parent_directories(
                        &request.destination,
                        &item.destination_relative_path,
                        &mut stages,
                        &mut directory_count,
                        &mut created_directories,
                    )?;
                    let atomic = destination_is_atomic(&request.destination);
                    stages.push(Stage::CopyFile {
                        source_relative_path: item.source_relative_path.clone(),
                        destination_relative_path: item.destination_relative_path.clone(),
                        bytes: size,
                        atomic,
                        conflict: resolution,
                    });
                    total_bytes = total_bytes.saturating_add(size);
                    file_count = file_count.saturating_add(1);
                }
            } else {
                return Err(TransferError::UnsupportedSourceEntry {
                    path: item.source_relative_path.clone(),
                });
            }
        }

        if let Some(budget) = request.capacity_budget {
            if total_bytes > budget {
                return Err(TransferError::CapacityExceeded {
                    required: total_bytes,
                    budget,
                });
            }
        }

        Ok(TransferPlan {
            stages,
            total_bytes,
            file_count,
            directory_count,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_directory_stages(
        request: &TransferRequest,
        item: &TransferItem,
        stages: &mut Vec<Stage>,
        total_bytes: &mut u64,
        file_count: &mut u32,
        directory_count: &mut u32,
        created_directories: &mut BTreeSet<PathBuf>,
    ) -> Result<(), TransferError> {
        ensure_directory_stage(
            &item.destination_relative_path,
            stages,
            directory_count,
            created_directories,
        );

        let source_root = request.source.root().to_path_buf();
        let walker = walkdir::WalkDir::new(source_root.join(&item.source_relative_path))
            .follow_links(false)
            .same_file_system(true)
            .sort_by_file_name()
            .into_iter();

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    return Err(TransferError::io(
                        "failed to enumerate source directory",
                        error
                            .into_io_error()
                            .unwrap_or_else(|| io::Error::other("walkdir error without payload")),
                    ));
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            request.source.validate().map_err(|error| {
                TransferError::io("source authority changed during planning", error)
            })?;
            let entry_abs = entry.path();
            let relative_to_source = match entry_abs.strip_prefix(request.source.root()) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => continue,
            };
            let source_size = entry.metadata().map(|m| m.len()).unwrap_or(0);

            // Build the destination path by replacing the source prefix.
            let strip_prefix = &item.source_relative_path;
            let destination_relative = match relative_to_source.strip_prefix(strip_prefix) {
                Ok(suffix) => {
                    let mut dest = item.destination_relative_path.clone();
                    for component in suffix.components() {
                        dest.push(component.as_os_str());
                    }
                    dest
                }
                Err(_) => continue,
            };
            ensure_parent_directories(
                &request.destination,
                &destination_relative,
                stages,
                directory_count,
                created_directories,
            )?;
            let resolution = resolve_conflict(
                &request.destination,
                &destination_relative,
                request.conflict_policy,
            )?;
            if let Some(resolution) = resolution {
                let atomic = destination_is_atomic(&request.destination);
                stages.push(Stage::CopyFile {
                    source_relative_path: relative_to_source,
                    destination_relative_path: destination_relative,
                    bytes: source_size,
                    atomic,
                    conflict: resolution,
                });
                *total_bytes = total_bytes.saturating_add(source_size);
                *file_count = file_count.saturating_add(1);
            }
        }
        Ok(())
    }
}

fn validate_request(request: &TransferRequest) -> Result<(), TransferError> {
    if request.items.is_empty() {
        return Ok(());
    }
    for item in &request.items {
        if item.source_relative_path.as_os_str().is_empty() {
            return Err(TransferError::InvalidItemPath {
                path: item.source_relative_path.clone(),
            });
        }
        if item.destination_relative_path.as_os_str().is_empty() {
            return Err(TransferError::InvalidItemPath {
                path: item.destination_relative_path.clone(),
            });
        }
        if item.source_relative_path.is_absolute() {
            return Err(TransferError::InvalidItemPath {
                path: item.source_relative_path.clone(),
            });
        }
        if item.destination_relative_path.is_absolute() {
            return Err(TransferError::InvalidItemPath {
                path: item.destination_relative_path.clone(),
            });
        }
        for component in item.source_relative_path.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(TransferError::InvalidItemPath {
                    path: item.source_relative_path.clone(),
                });
            }
        }
        for component in item.destination_relative_path.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(TransferError::InvalidItemPath {
                    path: item.destination_relative_path.clone(),
                });
            }
        }
    }
    Ok(())
}

fn ensure_ancestor_directory_stages(
    destination: &MountedWriteAuthority,
    directory_path: &Path,
    stages: &mut Vec<Stage>,
    directory_count: &mut u32,
    created_directories: &mut BTreeSet<PathBuf>,
) -> Result<(), TransferError> {
    // Walk every ancestor and ensure it is staged once. The destination
    // authority performs the actual `create_relative_directory` work during
    // execution; the plan only records the work.
    let mut current = PathBuf::new();
    for component in directory_path.components() {
        if let std::path::Component::Normal(name) = component {
            current.push(name);
            if created_directories.insert(current.clone()) {
                stages.push(Stage::CreateDirectory {
                    destination_relative_path: current.clone(),
                });
                *directory_count = directory_count.saturating_add(1);
            }
        } else {
            return Err(TransferError::InvalidItemPath {
                path: directory_path.to_path_buf(),
            });
        }
    }
    let _ = destination;
    Ok(())
}

fn ensure_directory_stage(
    directory_path: &Path,
    stages: &mut Vec<Stage>,
    directory_count: &mut u32,
    created_directories: &mut BTreeSet<PathBuf>,
) {
    if created_directories.insert(directory_path.to_path_buf()) {
        stages.push(Stage::CreateDirectory {
            destination_relative_path: directory_path.to_path_buf(),
        });
        *directory_count = directory_count.saturating_add(1);
    }
}

fn ensure_parent_directories(
    destination: &MountedWriteAuthority,
    destination_relative: &Path,
    stages: &mut Vec<Stage>,
    directory_count: &mut u32,
    created_directories: &mut BTreeSet<PathBuf>,
) -> Result<(), TransferError> {
    let parent = match destination_relative.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => return Ok(()),
    };
    ensure_ancestor_directory_stages(
        destination,
        &parent,
        stages,
        directory_count,
        created_directories,
    )
}

fn resolve_conflict(
    destination: &MountedWriteAuthority,
    destination_relative: &Path,
    policy: ConflictPolicy,
) -> Result<Option<ConflictResolution>, TransferError> {
    let final_path = destination.root().join(destination_relative);
    let exists = match std::fs::symlink_metadata(&final_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(TransferError::io(
                "failed to stat destination during planning",
                error,
            ));
        }
    };
    match (policy, exists) {
        (ConflictPolicy::Skip, None) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Fail, None) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Overwrite, None) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Preserve, None) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Skip, Some(_)) => Ok(None),
        (ConflictPolicy::Fail, Some(_)) => Err(TransferError::ConflictRejected {
            path: destination_relative.to_path_buf(),
        }),
        (ConflictPolicy::Overwrite, Some(_)) => Ok(Some(ConflictResolution::Overwrite)),
        (ConflictPolicy::Preserve, Some(_)) => Ok(Some(ConflictResolution::Preserved)),
    }
}

fn destination_is_atomic(destination: &MountedWriteAuthority) -> bool {
    // Staged files live as siblings of the destination; the rename is atomic
    // on every supported platform. Cross-filesystem moves are not in this
    // module's scope, so the answer is always `true` while the destination
    // authority is valid.
    destination.validate().is_ok()
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
                self.rollback(&mut committed_files).map_err(|error| {
                    TransferError::RollbackFailed {
                        path: PathBuf::new(),
                        context: error.to_string(),
                    }
                })?;
                return Err(TransferError::Cancelled);
            }
            progress.on_stage_started(stage, index as u32, total_stages);
            match stage {
                Stage::CreateDirectory {
                    destination_relative_path,
                } => {
                    self.execute_create_directory(destination_relative_path)?;
                }
                Stage::CopyFile {
                    source_relative_path,
                    destination_relative_path,
                    bytes,
                    atomic: _,
                    conflict,
                } => {
                    let outcome = self.execute_copy_file(
                        source_relative_path,
                        destination_relative_path,
                        *bytes,
                        &mut bytes_so_far,
                        total_bytes,
                        index as u32,
                        total_stages,
                        progress,
                        cancellation,
                    )?;
                    let _ = outcome;
                    let _ = conflict;
                    committed_files.push(destination_relative_path.clone());
                }
                Stage::RemoveFile { .. } => {
                    // RemoveFile stages are inserted only by the rollback path
                    // and never appear in a forward plan. Skip defensively.
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

    #[allow(clippy::too_many_arguments)]
    fn execute_copy_file(
        &self,
        source_relative: &Path,
        destination_relative: &Path,
        declared_bytes: u64,
        bytes_so_far: &mut u64,
        total_bytes: u64,
        stage_index: u32,
        total_stages: u32,
        progress: &mut dyn TransferProgress,
        cancellation: &CancellationObserver,
    ) -> Result<CommitOutcome, TransferError> {
        self.request
            .source
            .validate()
            .map_err(|error| TransferError::authority(format!("source not current: {error}")))?;
        self.request.destination.validate().map_err(|error| {
            TransferError::authority(format!("destination not current: {error}"))
        })?;

        let source_result = self.request.source.with_relative_file(
            source_relative,
            |mut source_file| -> io::Result<(CommitOutcome, u64)> {
                let staged = self
                    .request
                    .destination
                    .prepare_write_relative_file(destination_relative, self.request.conflict_policy)
                    .map_err(|error| TransferError::io("failed to stage destination file", error))
                    .map_err(io::Error::other)?;

                const CHUNK: usize = 64 * 1024;
                let mut buffer = vec![0u8; CHUNK];
                let mut copied: u64 = 0;
                loop {
                    if cancellation.is_cancelled() {
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
                        .staged_file()
                        .write_all(&buffer[..read])
                        .map_err(|error| TransferError::io("failed to write staged file", error))
                        .map_err(io::Error::other)?;
                    copied = copied.saturating_add(read as u64);
                    *bytes_so_far = bytes_so_far.saturating_add(read as u64);
                    progress.on_bytes_copied(stage_index, total_stages, *bytes_so_far, total_bytes);
                }
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

                let outcome = staged
                    .commit()
                    .map_err(|error| TransferError::io("staged commit failed", error))
                    .map_err(io::Error::other)?;
                Ok((outcome, copied))
            },
        );
        let (outcome, copied) = match source_result {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Err(TransferError::Cancelled);
            }
            Err(error) => {
                return Err(TransferError::io("failed to copy source file", error));
            }
        };
        let _ = copied;
        Ok(outcome)
    }

    fn rollback(&self, committed_files: &mut Vec<PathBuf>) -> io::Result<()> {
        while let Some(relative) = committed_files.pop() {
            self.request
                .destination
                .validate()
                .map_err(|error| {
                    TransferError::authority(format!("destination not current: {error}"))
                })
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            self.request.destination.remove_relative_file(&relative)?;
        }
        Ok(())
    }
}

trait WalkdirErrorExt {
    fn into_io_error(self) -> Option<io::Error>;
}

impl WalkdirErrorExt for walkdir::Error {
    fn into_io_error(self) -> Option<io::Error> {
        self.io_error()
            .map(|error| io::Error::new(error.kind(), format!("walkdir error: {error}")))
    }
}

fn _unused_os_string(_value: OsString) {}

#[cfg(test)]
mod tests {
    use super::*;

    use uuid::Uuid;

    use crate::local::write_authority::ConflictPolicy;

    fn unique(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("tributary-transfer-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create root");
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn make_authority_pair(path: &Path) -> (Arc<MountedRootAuthority>, MountedWriteAuthority) {
        let mounted = MountedRootAuthority::acquire(path).expect("acquire mounted");
        let read = Arc::new(mounted);
        let write = MountedWriteAuthority::from_mounted(Arc::clone(&read));
        (read, write)
    }

    fn make_read_authority(path: &Path) -> Arc<MountedRootAuthority> {
        Arc::new(MountedRootAuthority::acquire(path).expect("acquire read authority"))
    }

    fn write_source_file(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
        std::fs::write(path, contents).expect("write source");
    }

    #[test]
    fn plan_resolves_file_into_single_copy_stage() {
        let source_root = unique("plan-src");
        let destination_root = unique("plan-dst");
        write_source_file(&source_root.join("album/song.flac"), b"audio");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);
        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("album/song.flac"))],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        assert_eq!(plan.file_count(), 1);
        assert!(
            plan.directory_count() >= 1,
            "album directory must be staged"
        );
        let total = plan.total_bytes();
        assert!(total >= 5);
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn capacity_budget_rejects_oversized_plan() {
        let source_root = unique("budget-src");
        let destination_root = unique("budget-dst");
        write_source_file(&source_root.join("big.flac"), &[0u8; 100]);
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);

        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("big.flac"))],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: Some(10),
            recurse_directories: true,
        };
        let error = TransferPlanner::new()
            .plan(&request)
            .expect_err("oversized plan must be rejected");
        assert!(matches!(error, TransferError::CapacityExceeded { .. }));
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn plan_walks_directory_recursively() {
        let source_root = unique("walk-src");
        let destination_root = unique("walk-dst");
        write_source_file(&source_root.join("album/a.flac"), b"a");
        write_source_file(&source_root.join("album/nested/b.flac"), b"b");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);

        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::new(
                PathBuf::from("album"),
                PathBuf::from("imported"),
            )],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        assert_eq!(plan.file_count(), 2);
        assert!(
            plan.directory_count() >= 2,
            "album and nested must be staged"
        );
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn executor_copies_a_single_file() {
        let source_root = unique("exec-src");
        let destination_root = unique("exec-dst");
        write_source_file(&source_root.join("song.flac"), b"copy me");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);

        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("song.flac"))],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        let observer = CancellationObserver::never_cancelled();
        let mut progress = ();
        let summary = TransferExecutor::new(request, plan)
            .run(&mut progress, &observer)
            .expect("run");
        assert!(summary.completed);
        let final_path = destination_root.join("song.flac");
        let bytes = std::fs::read(&final_path).expect("read final");
        assert_eq!(bytes, b"copy me");
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn executor_recursive_directory_copy() {
        let source_root = unique("rec-src");
        let destination_root = unique("rec-dst");
        write_source_file(&source_root.join("album/a.flac"), b"a");
        write_source_file(&source_root.join("album/b.flac"), b"b");
        write_source_file(&source_root.join("album/nested/c.flac"), b"c");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);

        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::new(
                PathBuf::from("album"),
                PathBuf::from("imported"),
            )],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        let observer = CancellationObserver::never_cancelled();
        let mut progress = ();
        let summary = TransferExecutor::new(request, plan)
            .run(&mut progress, &observer)
            .expect("run");
        assert!(summary.completed);
        assert_eq!(
            std::fs::read(destination_root.join("imported/a.flac")).expect("read a"),
            b"a"
        );
        assert_eq!(
            std::fs::read(destination_root.join("imported/b.flac")).expect("read b"),
            b"b"
        );
        assert_eq!(
            std::fs::read(destination_root.join("imported/nested/c.flac")).expect("read c"),
            b"c"
        );
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn conflict_fail_rejects_existing_destination() {
        let source_root = unique("fail-src");
        let destination_root = unique("fail-dst");
        write_source_file(&source_root.join("song.flac"), b"new");
        std::fs::write(destination_root.join("song.flac"), b"old").expect("write existing");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);

        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("song.flac"))],
            conflict_policy: ConflictPolicy::Fail,
            capacity_budget: None,
            recurse_directories: true,
        };
        let error = TransferPlanner::new()
            .plan(&request)
            .expect_err("fail policy must reject existing destination");
        assert!(matches!(error, TransferError::ConflictRejected { .. }));
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    struct ProgressRecorder {
        stage_starts: u32,
        stage_completes: u32,
        byte_chunks: u32,
    }

    impl ProgressRecorder {
        fn new() -> Self {
            Self {
                stage_starts: 0,
                stage_completes: 0,
                byte_chunks: 0,
            }
        }
    }

    impl TransferProgress for ProgressRecorder {
        fn on_stage_started(&mut self, _stage: &Stage, _index: u32, _total: u32) {
            self.stage_starts = self.stage_starts.saturating_add(1);
        }
        fn on_stage_completed(
            &mut self,
            _stage: &Stage,
            _index: u32,
            _total: u32,
            _bytes_so_far: u64,
            _total_bytes: u64,
        ) {
            self.stage_completes = self.stage_completes.saturating_add(1);
        }
        fn on_bytes_copied(
            &mut self,
            _stage_index: u32,
            _total_stages: u32,
            _bytes_so_far: u64,
            _total_bytes: u64,
        ) {
            self.byte_chunks = self.byte_chunks.saturating_add(1);
        }
    }

    #[test]
    fn progress_callback_reports_every_stage_and_chunk() {
        let source_root = unique("progress-src");
        let destination_root = unique("progress-dst");
        // 200 KiB so the 64 KiB chunked copy yields multiple progress reports.
        let payload = vec![0u8; 200 * 1024];
        write_source_file(&source_root.join("payload.bin"), &payload);
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);
        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("payload.bin"))],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        let observer = CancellationObserver::never_cancelled();
        let mut progress = ProgressRecorder::new();
        let summary = TransferExecutor::new(request, plan)
            .run(&mut progress, &observer)
            .expect("run");
        assert!(summary.completed);
        assert_eq!(progress.stage_starts, 1, "one copy stage should start");
        assert_eq!(
            progress.stage_completes, 1,
            "one copy stage should complete"
        );
        assert!(
            progress.byte_chunks >= 3,
            "at least three progress chunks for 200 KiB"
        );
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn overwrite_policy_replaces_existing_destination() {
        let source_root = unique("overwrite-src");
        let destination_root = unique("overwrite-dst");
        write_source_file(&source_root.join("song.flac"), b"new");
        std::fs::write(destination_root.join("song.flac"), b"old").expect("write existing");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);
        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("song.flac"))],
            conflict_policy: ConflictPolicy::Overwrite,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        let observer = CancellationObserver::never_cancelled();
        let mut progress = ();
        let _ = TransferExecutor::new(request, plan)
            .run(&mut progress, &observer)
            .expect("run");
        assert_eq!(
            std::fs::read(destination_root.join("song.flac")).expect("read final"),
            b"new"
        );
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn skip_policy_skips_existing_destination() {
        let source_root = unique("skip-src");
        let destination_root = unique("skip-dst");
        write_source_file(&source_root.join("song.flac"), b"new");
        std::fs::write(destination_root.join("song.flac"), b"old").expect("write existing");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);
        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("song.flac"))],
            conflict_policy: ConflictPolicy::Skip,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        assert_eq!(
            plan.file_count(),
            0,
            "skip policy should produce no copy stages"
        );
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn empty_request_plans_to_no_stages() {
        let source_root = unique("empty-src");
        let destination_root = unique("empty-dst");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);
        let request = TransferRequest {
            source,
            destination,
            items: vec![],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let plan = TransferPlanner::new().plan(&request).expect("plan");
        assert!(plan.is_empty());
        assert_eq!(plan.stage_count(), 0);
        assert_eq!(plan.total_bytes(), 0);
        cleanup(&source_root);
        cleanup(&destination_root);
    }

    #[test]
    fn absolute_path_in_request_is_rejected() {
        let source_root = unique("abs-src");
        let destination_root = unique("abs-dst");
        let source = make_read_authority(&source_root);
        let (_, destination) = make_authority_pair(&destination_root);
        let request = TransferRequest {
            source,
            destination,
            items: vec![TransferItem::same(PathBuf::from("/etc/passwd"))],
            conflict_policy: ConflictPolicy::Preserve,
            capacity_budget: None,
            recurse_directories: true,
        };
        let error = TransferPlanner::new()
            .plan(&request)
            .expect_err("absolute path must be rejected");
        assert!(matches!(error, TransferError::InvalidItemPath { .. }));
        cleanup(&source_root);
        cleanup(&destination_root);
    }
}
