//! Transfer planning: resolve a request into an ordered stage list.
//!
//! Planning is read-only against the source and destination authorities.
//! It opens the source to confirm each regular file's size but does not
//! stage any writes. The destination is queried for existing entries to
//! resolve conflict policy; the resolved policy is recorded on every copy
//! stage so the executor never re-decides a conflict.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use super::{ConflictResolution, Stage, TransferError, TransferItem, TransferPlan, TransferRequest};
use crate::local::write_authority::{ConflictPolicy, MountedWriteAuthority};

/// Running tally of stages and counts accumulated while planning.
#[derive(Default)]
struct PlanAccumulator {
    stages: Vec<Stage>,
    total_bytes: u64,
    file_count: u32,
    directory_count: u32,
    created_directories: BTreeSet<PathBuf>,
}

impl PlanAccumulator {
    /// Record one directory-creation stage unless it was already staged.
    fn ensure_directory_stage(&mut self, directory_path: &Path) {
        if self.created_directories.insert(directory_path.to_path_buf()) {
            self.stages.push(Stage::CreateDirectory {
                destination_relative_path: directory_path.to_path_buf(),
            });
            self.directory_count = self.directory_count.saturating_add(1);
        }
    }

    /// Record one file-copy stage and its byte/count contributions.
    fn push_file_stage(
        &mut self,
        source_relative_path: PathBuf,
        destination_relative_path: PathBuf,
        bytes: u64,
        atomic: bool,
        conflict: ConflictResolution,
    ) {
        self.stages.push(Stage::CopyFile {
            source_relative_path,
            destination_relative_path,
            bytes,
            atomic,
            conflict,
        });
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.file_count = self.file_count.saturating_add(1);
    }

    /// Stage every ancestor directory of a destination path once.
    ///
    /// The destination authority performs the actual
    /// `create_relative_directory` work during execution; the plan only
    /// records the work.
    fn ensure_parent_directories(&mut self, destination_relative: &Path) -> Result<(), TransferError> {
        let parent = match destination_relative.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => return Ok(()),
        };
        let mut current = PathBuf::new();
        for component in parent.components() {
            if let std::path::Component::Normal(name) = component {
                current.push(name);
                self.ensure_directory_stage(&current);
            } else {
                return Err(TransferError::InvalidItemPath {
                    path: parent.to_path_buf(),
                });
            }
        }
        Ok(())
    }
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
    /// Items are planned in request order so callers can express
    /// playlist-order or directory-recursion intent.
    #[allow(clippy::unused_self)]
    pub fn plan(&self, request: &TransferRequest) -> Result<TransferPlan, TransferError> {
        validate_request(request)?;

        let mut accumulator = PlanAccumulator::default();
        for item in &request.items {
            Self::plan_item(request, item, &mut accumulator)?;
        }

        if let Some(budget) = request.capacity_budget {
            if accumulator.total_bytes > budget {
                return Err(TransferError::CapacityExceeded {
                    required: accumulator.total_bytes,
                    budget,
                });
            }
        }

        Ok(TransferPlan {
            stages: accumulator.stages,
            total_bytes: accumulator.total_bytes,
            file_count: accumulator.file_count,
            directory_count: accumulator.directory_count,
        })
    }

    /// Plan one request item: verify it, then expand a directory
    /// recursively or stage a single file copy.
    fn plan_item(
        request: &TransferRequest,
        item: &TransferItem,
        accumulator: &mut PlanAccumulator,
    ) -> Result<(), TransferError> {
        validate_item(item)?;
        request.source.validate().map_err(|error| {
            TransferError::io("source authority failed pre-plan validation", error)
        })?;
        request.destination.validate().map_err(|error| {
            TransferError::io("destination authority failed pre-plan validation", error)
        })?;

        let source_abs = request.source.root().join(&item.source_relative_path);
        let source_meta = read_source_metadata(&source_abs, item)?;

        if source_meta.is_dir() {
            if request.recurse_directories {
                collect_directory_stages(request, item, accumulator)?;
            } else {
                accumulator.ensure_directory_stage(&item.destination_relative_path);
            }
        } else if source_meta.is_file() {
            plan_file_item(request, item, source_meta.len(), accumulator)?;
        } else {
            return Err(TransferError::UnsupportedSourceEntry {
                path: item.source_relative_path.clone(),
            });
        }
        Ok(())
    }
}

/// Reject empty, absolute, or escaping relative paths in an item.
fn validate_item(item: &TransferItem) -> Result<(), TransferError> {
    ensure_relative_path(&item.source_relative_path)?;
    ensure_relative_path(&item.destination_relative_path)?;
    Ok(())
}

fn ensure_relative_path(path: &Path) -> Result<(), TransferError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(TransferError::InvalidItemPath {
            path: path.to_path_buf(),
        });
    }
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(TransferError::InvalidItemPath {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn validate_request(request: &TransferRequest) -> Result<(), TransferError> {
    for item in &request.items {
        validate_item(item)?;
    }
    Ok(())
}

/// Read the source entry's metadata, mapping `NotFound` to the typed
/// unsupported-entry error and anything else to an I/O error.
fn read_source_metadata(
    source_abs: &Path,
    item: &TransferItem,
) -> Result<std::fs::Metadata, TransferError> {
    match std::fs::symlink_metadata(source_abs) {
        Ok(metadata) => Ok(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(TransferError::UnsupportedSourceEntry {
                path: item.source_relative_path.clone(),
            })
        }
        Err(error) => Err(TransferError::io(
            "failed to read source entry metadata",
            error,
        )),
    }
}

/// Stage one regular-file item: resolve the conflict policy and record
/// the copy stage together with its parent-directory stages.
fn plan_file_item(
    request: &TransferRequest,
    item: &TransferItem,
    size: u64,
    accumulator: &mut PlanAccumulator,
) -> Result<(), TransferError> {
    let resolution = resolve_conflict(
        &request.destination,
        &item.destination_relative_path,
        request.conflict_policy,
    )?;
    if let Some(resolution) = resolution {
        accumulator.ensure_parent_directories(&item.destination_relative_path)?;
        let atomic = destination_is_atomic(&request.destination);
        accumulator.push_file_stage(
            item.source_relative_path.clone(),
            item.destination_relative_path.clone(),
            size,
            atomic,
            resolution,
        );
    }
    Ok(())
}

/// Expand a directory item into per-file copy stages by walking the
/// source subtree.
fn collect_directory_stages(
    request: &TransferRequest,
    item: &TransferItem,
    accumulator: &mut PlanAccumulator,
) -> Result<(), TransferError> {
    accumulator.ensure_directory_stage(&item.destination_relative_path);

    let source_root = request.source.root().to_path_buf();
    let walker = walkdir::WalkDir::new(source_root.join(&item.source_relative_path))
        .follow_links(false)
        .same_file_system(true)
        .sort_by_file_name()
        .into_iter();

    for entry in walker {
        let entry = entry.map_err(walkdir_error("failed to enumerate source directory"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        collect_file_entry(request, item, &entry, accumulator)?;
    }
    Ok(())
}

/// Stage one walked regular-file entry beneath a directory item.
fn collect_file_entry(
    request: &TransferRequest,
    item: &TransferItem,
    entry: &walkdir::DirEntry,
    accumulator: &mut PlanAccumulator,
) -> Result<(), TransferError> {
    request.source.validate().map_err(|error| {
        TransferError::io("source authority changed during planning", error)
    })?;
    let relative_to_source = match entry.path().strip_prefix(request.source.root()) {
        Ok(relative) => relative.to_path_buf(),
        // The entry escaped the source root; nothing to transfer.
        Err(_) => return Ok(()),
    };
    // A metadata failure must not silently become a zero-byte
    // stage: that would let a plan slip past the capacity
    // budget and skip the post-copy size check.
    let source_size = entry
        .metadata()
        .map_err(walkdir_error(
            "failed to read source entry metadata during planning",
        ))?
        .len();

    // Build the destination path by replacing the source prefix.
    let destination_relative = match relative_to_source.strip_prefix(&item.source_relative_path) {
        Ok(suffix) => {
            let mut dest = item.destination_relative_path.clone();
            for component in suffix.components() {
                dest.push(component.as_os_str());
            }
            dest
        }
        Err(_) => return Ok(()),
    };
    accumulator.ensure_parent_directories(&destination_relative)?;
    let resolution = resolve_conflict(
        &request.destination,
        &destination_relative,
        request.conflict_policy,
    )?;
    if let Some(resolution) = resolution {
        let atomic = destination_is_atomic(&request.destination);
        accumulator.push_file_stage(
            relative_to_source,
            destination_relative,
            source_size,
            atomic,
            resolution,
        );
    }
    Ok(())
}

/// Convert a walkdir error into the module's typed I/O error. A walkdir
/// error may or may not carry an I/O payload; the fallback keeps the
/// error chain intact either way.
fn walkdir_error(context: &'static str) -> impl Fn(walkdir::Error) -> TransferError {
    move |error| {
        TransferError::io(
            context,
            error
                .io_error()
                .map(|io_error| {
                    io::Error::new(io_error.kind(), format!("walkdir error: {io_error}"))
                })
                .unwrap_or_else(|| io::Error::other("walkdir error without payload")),
        )
    }
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
