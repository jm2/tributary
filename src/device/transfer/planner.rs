//! The read-only transfer planner.
//!
//! Planning opens the source to confirm each regular file's size but does not
//! stage any writes. The destination is queried for existing entries to
//! resolve conflict policy; the resolved policy is recorded on every copy
//! stage so the executor never re-decides a conflict.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use super::types::{Stage, TransferError, TransferItem, TransferPlan, TransferRequest};
use crate::local::root_authority::CrossedMountBoundary;
use crate::local::write_authority::{ConflictPolicy, ConflictResolution, MountedWriteAuthority};

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
        let mut builder = PlanBuilder::new(request);
        for item in &request.items {
            builder.plan_item(item)?;
        }
        builder.finish()
    }
}

/// Incrementally assembles a [`TransferPlan`] from a [`TransferRequest`],
/// carrying the running totals and the set of directories already staged so
/// every stage is emitted exactly once.
struct PlanBuilder<'a> {
    request: &'a TransferRequest,
    stages: Vec<Stage>,
    total_bytes: u64,
    file_count: u32,
    directory_count: u32,
    created_directories: BTreeSet<PathBuf>,
}

impl<'a> PlanBuilder<'a> {
    fn new(request: &'a TransferRequest) -> Self {
        Self {
            request,
            stages: Vec::new(),
            total_bytes: 0,
            file_count: 0,
            directory_count: 0,
            created_directories: BTreeSet::new(),
        }
    }

    /// Plan one request item: authority validation, then dispatch on the
    /// source entry's file type. Item paths were fully validated (non-empty,
    /// relative, normal components only) for the whole request by
    /// [`validate_request`] before planning began.
    fn plan_item(&mut self, item: &TransferItem) -> Result<(), TransferError> {
        self.validate_authorities()?;
        let source_absolute = self.request.source.root().join(&item.source_relative_path);
        let metadata = read_source_metadata(&source_absolute, &item.source_relative_path)?;
        if metadata.is_dir() {
            // A directory item mapped to a nested destination must stage
            // every missing ancestor before its leaf, exactly once each:
            // the executor records each created component for rollback, so
            // a failed transfer removes the whole created chain instead of
            // leaving ancestors behind.
            ensure_ancestor_directory_stages(
                &item.destination_relative_path,
                &mut self.stages,
                &mut self.directory_count,
                &mut self.created_directories,
            )?;
            if self.request.recurse_directories {
                self.collect_directory_stages(item)?;
            }
        } else if metadata.is_file() {
            self.ensure_source_file_boundary(&source_absolute)?;
            self.plan_file_item(item, metadata.len())?;
        } else {
            return Err(TransferError::UnsupportedSourceEntry {
                path: item.source_relative_path.clone(),
            });
        }
        Ok(())
    }

    /// Revalidate both authorities before planning against them.
    fn validate_authorities(&self) -> Result<(), TransferError> {
        self.request.source.validate().map_err(|error| {
            TransferError::io("source authority failed pre-plan validation", error)
        })?;
        self.request.destination.validate().map_err(|error| {
            TransferError::io("destination authority failed pre-plan validation", error)
        })?;
        Ok(())
    }

    /// Plan a single regular-file item, honouring the conflict policy.
    fn plan_file_item(&mut self, item: &TransferItem, size: u64) -> Result<(), TransferError> {
        let Some(resolution) = resolve_conflict(
            &self.request.destination,
            &item.destination_relative_path,
            self.request.conflict_policy,
        )?
        else {
            return Ok(());
        };
        self.ensure_parent_directories(&item.destination_relative_path)?;
        let atomic = destination_is_atomic(&self.request.destination);
        self.push_copy_stage(
            item.source_relative_path.clone(),
            item.destination_relative_path.clone(),
            size,
            resolution,
            atomic,
        );
        Ok(())
    }

    /// Walk a directory item recursively and plan a copy stage for every
    /// contained regular file.
    fn collect_directory_stages(&mut self, item: &TransferItem) -> Result<(), TransferError> {
        let source_root = self.request.source.root().to_path_buf();
        let walker = walkdir::WalkDir::new(source_root.join(&item.source_relative_path))
            .follow_links(false)
            .same_file_system(true)
            .sort_by_file_name()
            .into_iter();
        for entry in walker {
            let entry = entry.map_err(|error| {
                TransferError::io(
                    "failed to enumerate source directory",
                    walkdir_io_error(error),
                )
            })?;
            if entry.file_type().is_dir() {
                self.ensure_walked_directory_boundary(entry.path())?;
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            self.ensure_source_file_boundary(entry.path())?;
            self.plan_walked_file(item, &entry)?;
        }
        Ok(())
    }

    /// Validate one walked directory's mount boundary BEFORE any child of
    /// it is staged. The walk's `st_dev` comparison cannot see a same-device
    /// Linux bind mount, while the executor's per-component mount-ID checks
    /// refuse every file beneath one — a nested-mount transfer could never
    /// complete, and the refusal would land mid-run after earlier stages
    /// had already committed, with the mount-foreign bytes already counted
    /// into the plan. A boundary crossing therefore rejects the whole plan
    /// at planning time with the nested mount path named; non-mount trees
    /// stage and account exactly as before.
    fn ensure_walked_directory_boundary(&self, directory: &Path) -> Result<(), TransferError> {
        self.request
            .source
            .validate_walked_directory_boundary(directory)
            .map_err(|error| {
                source_boundary_error("failed to validate source directory mount boundary", error)
            })
    }

    /// Validate one planned source file's mount boundary BEFORE it is staged.
    /// The walk's `st_dev` comparison cannot see a same-device Linux bind
    /// mount of a regular file, nor one of any directory on the file's path,
    /// while the executor's per-component mount-ID checks refuse the file —
    /// after earlier stages had already committed and with the mount-foreign
    /// bytes already counted into the plan. Probing every planned file (walked
    /// entries and directly requested items) rejects the whole plan at
    /// planning time with the offending path named; non-mount trees stage and
    /// account exactly as before.
    fn ensure_source_file_boundary(&self, file: &Path) -> Result<(), TransferError> {
        self.request
            .source
            .validate_source_file_boundary(file)
            .map_err(|error| {
                source_boundary_error("failed to validate source file mount boundary", error)
            })
    }

    /// Plan one file discovered by the directory walk, honouring the
    /// conflict policy exactly like [`PlanBuilder::plan_file_item`]: the
    /// conflict is resolved first so a skipped file stages no parent
    /// directory work, and only a file that will actually be copied emits
    /// its `CreateDirectory` stages.
    fn plan_walked_file(
        &mut self,
        item: &TransferItem,
        entry: &walkdir::DirEntry,
    ) -> Result<(), TransferError> {
        self.request.source.validate().map_err(|error| {
            TransferError::io("source authority changed during planning", error)
        })?;
        let source_relative = match entry.path().strip_prefix(self.request.source.root()) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => return Ok(()),
        };
        let bytes = entry
            .metadata()
            .map_err(|error| {
                TransferError::io(
                    "failed to read source entry metadata",
                    walkdir_io_error(error),
                )
            })?
            .len();
        let Some(destination_relative) = destination_for_source_path(item, &source_relative) else {
            return Ok(());
        };
        let Some(resolution) = resolve_conflict(
            &self.request.destination,
            &destination_relative,
            self.request.conflict_policy,
        )?
        else {
            return Ok(());
        };
        self.ensure_parent_directories(&destination_relative)?;
        let atomic = destination_is_atomic(&self.request.destination);
        self.push_copy_stage(
            source_relative,
            destination_relative,
            bytes,
            resolution,
            atomic,
        );
        Ok(())
    }

    /// Record one copy stage and advance the running totals.
    fn push_copy_stage(
        &mut self,
        source_relative_path: PathBuf,
        destination_relative_path: PathBuf,
        bytes: u64,
        conflict: ConflictResolution,
        atomic: bool,
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

    /// Stage the parent directory chain of `destination_relative`, once each.
    fn ensure_parent_directories(
        &mut self,
        destination_relative: &Path,
    ) -> Result<(), TransferError> {
        let parent = match destination_relative.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => return Ok(()),
        };
        ensure_ancestor_directory_stages(
            &parent,
            &mut self.stages,
            &mut self.directory_count,
            &mut self.created_directories,
        )
    }

    /// Apply the capacity budget and freeze the accumulated plan.
    fn finish(self) -> Result<TransferPlan, TransferError> {
        if let Some(budget) = self.request.capacity_budget {
            if self.total_bytes > budget {
                return Err(TransferError::CapacityExceeded {
                    required: self.total_bytes,
                    budget,
                });
            }
        }
        Ok(TransferPlan {
            stages: self.stages,
            total_bytes: self.total_bytes,
            file_count: self.file_count,
            directory_count: self.directory_count,
        })
    }
}

/// Convert a source-boundary probe failure into the typed plan rejection,
/// preserving the named nested-mount path when the probe crossed one.
fn source_boundary_error(context: &str, error: io::Error) -> TransferError {
    if let Some(crossing) = error
        .get_ref()
        .and_then(|payload| payload.downcast_ref::<CrossedMountBoundary>())
    {
        return TransferError::NestedMountBoundary {
            path: crossing.path.clone(),
        };
    }
    TransferError::io(context, error)
}

/// Validate every item path in the request: non-empty, relative, and made of
/// normal components only.
fn validate_request(request: &TransferRequest) -> Result<(), TransferError> {
    for item in &request.items {
        validate_item_path(&item.source_relative_path)?;
        validate_item_path(&item.destination_relative_path)?;
    }
    Ok(())
}

/// Validate one relative path for use as a transfer endpoint.
fn validate_item_path(relative: &Path) -> Result<(), TransferError> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(TransferError::InvalidItemPath {
            path: relative.to_path_buf(),
        });
    }
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(TransferError::InvalidItemPath {
                path: relative.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Read the symlink metadata of a source entry, mapping `NotFound` to the
/// typed unsupported-entry error.
fn read_source_metadata(
    absolute: &Path,
    relative: &Path,
) -> Result<std::fs::Metadata, TransferError> {
    match std::fs::symlink_metadata(absolute) {
        Ok(metadata) => Ok(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(TransferError::UnsupportedSourceEntry {
                path: relative.to_path_buf(),
            })
        }
        Err(error) => Err(TransferError::io(
            "failed to read source entry metadata",
            error,
        )),
    }
}

/// Map a walked source path to its destination path by replacing the item's
/// source prefix with the item's destination prefix. Returns `None` when the
/// path is outside the item's source subtree.
fn destination_for_source_path(item: &TransferItem, source_relative: &Path) -> Option<PathBuf> {
    let suffix = source_relative
        .strip_prefix(&item.source_relative_path)
        .ok()?;
    let mut destination = item.destination_relative_path.clone();
    for component in suffix.components() {
        destination.push(component.as_os_str());
    }
    Some(destination)
}

/// Stage every ancestor directory of `directory_path` exactly once. The
/// destination authority performs the actual `create_relative_directory`
/// work during execution; the plan only records the work.
fn ensure_ancestor_directory_stages(
    directory_path: &Path,
    stages: &mut Vec<Stage>,
    directory_count: &mut u32,
    created_directories: &mut BTreeSet<PathBuf>,
) -> Result<(), TransferError> {
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
    Ok(())
}

/// Resolve the conflict policy for one destination against the live
/// filesystem. `Ok(None)` means the stage should be skipped entirely.
///
/// The destination is probed through the retained [`MountedWriteAuthority`]
/// with per-component no-follow traversal, exactly as execution traverses
/// it, so a symlink/reparse-point ancestor can neither redirect the probe
/// outside the retained destination root (making a same-named external
/// entry look like the requested destination) nor yield a plan the
/// executor's no-follow traversal would reject only mid-run.
fn resolve_conflict(
    destination: &MountedWriteAuthority,
    destination_relative: &Path,
    policy: ConflictPolicy,
) -> Result<Option<ConflictResolution>, TransferError> {
    let exists = destination
        .relative_leaf_exists(destination_relative)
        .map_err(|error| TransferError::io("failed to probe destination during planning", error))?;
    match (policy, exists) {
        (ConflictPolicy::Skip, false) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Fail, false) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Overwrite, false) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Preserve, false) => Ok(Some(ConflictResolution::Fresh)),
        (ConflictPolicy::Skip, true) => Ok(None),
        (ConflictPolicy::Fail, true) => Err(TransferError::ConflictRejected {
            path: destination_relative.to_path_buf(),
        }),
        (ConflictPolicy::Overwrite, true) => Ok(Some(ConflictResolution::Overwrite)),
        (ConflictPolicy::Preserve, true) => Ok(Some(ConflictResolution::Preserved)),
    }
}

/// Whether a copy stage into this destination commits atomically. Staged
/// files live as siblings of the destination; the rename is atomic on every
/// supported platform. Cross-filesystem moves are not in this module's
/// scope, so the answer is always `true` while the destination authority is
/// valid.
fn destination_is_atomic(destination: &MountedWriteAuthority) -> bool {
    destination.validate().is_ok()
}

/// Flatten a [`walkdir::Error`] into an [`io::Error`], keeping the payload's
/// error kind when one is present.
fn walkdir_io_error(error: walkdir::Error) -> io::Error {
    error
        .io_error()
        .map(|io_error| io::Error::new(io_error.kind(), format!("walkdir error: {io_error}")))
        .unwrap_or_else(|| io::Error::other("walkdir error without payload"))
}
