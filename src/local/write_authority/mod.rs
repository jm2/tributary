//! Retained write authority for one exact mounted filesystem.
//!
//! This module adds write capability on top of the read-only
//! [`MountedRootAuthority`]. The same retained root handle, parent chain,
//! mount generation, and filesystem boundary policy gate every byte written
//! beneath the mount. There is no shared-marker requirement; mounted authority
//! exists for ephemeral portable devices and replaces its session epoch on
//! relocation, pre-unmount, or removal.
//!
//! The intended consumer is the generic mounted-filesystem transfer planner
//! in [`crate::device::transfer`]. It must never be used to author a library
//! root (those keep their marker-backed
//! [`RootAuthorityLease`](super::root_authority::RootAuthorityLease) and
//! explicit database enrollment).
//!
//! File writes are staged: a sibling temporary file is created with
//! `O_CREAT | O_EXCL | O_NOFOLLOW` (Unix) or with the reparse-point attribute
//! rejected (Windows), then renamed atomically once
//! [`PreparedWriteTarget::commit`] is called. A rollback drops the staged
//! file. The destination filesystem is observed through the same retained
//! boundary, so a binder swap or remount between staging and commit is
//! detected and refused without surfacing a partial publish.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::root_authority::MountedRootAuthority;

/// What the write authority should do when the destination of a write already
/// exists beneath the mount.
///
/// `Skip` and `Fail` close the question for the whole transfer on a single
/// collision; `Overwrite` and `Preserve` permit the operation to proceed
/// without further prompt. Each variant is a typed policy, not a boolean flag,
/// so reviewers can grep call sites for the precise behavior at every
/// admission boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictPolicy {
    /// Leave any existing destination untouched and skip the operation.
    Skip,
    /// Atomically replace the existing destination during commit.
    Overwrite,
    /// Choose a non-colliding name in the same directory and create anew.
    Preserve,
    /// Refuse the operation; transfer fails before any byte is written.
    Fail,
}

/// Outcome of resolving a conflict policy against the live filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictResolution {
    /// Destination was absent; the staged file becomes a fresh write.
    Fresh,
    /// Destination existed; the staged file will replace it on commit.
    Overwrite,
    /// Destination existed; the staged file is written to a disambiguated name.
    Preserved,
}

/// A staged write below a [`MountedWriteAuthority`] ready for commit/rollback.
///
/// The destination is held in a sibling temporary file. Until
/// [`commit`](Self::commit) is called the original destination is untouched,
/// so a partially-written staged file can be discarded without disturbing the
/// mount. Once commit fires, the rename is atomic on the same filesystem and
/// the staged file is gone.
pub struct PreparedWriteTarget {
    lease_token: Uuid,
    authority: Arc<MountedRootAuthority>,
    final_relative_path: PathBuf,
    /// Absolute path of the staged temporary file. Sibling of the destination
    /// so the rename is atomic on the same filesystem.
    staged_path: PathBuf,
    /// Retained handle on the staged file. Taken and dropped before publish
    /// or rollback: Windows refuses to rename or delete a path while any
    /// handle on the file is open unless that handle shares delete access,
    /// and the staged handle deliberately does not (it enforces exclusivity).
    /// POSIX rename and unlink are indifferent to open handles, so closing
    /// early is harmless there.
    staged_file: Option<File>,
    resolution: ConflictResolution,
    committed: bool,
}

impl fmt::Debug for PreparedWriteTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedWriteTarget")
            .field("target_relative_path", &self.final_relative_path)
            .field("staged_path", &self.staged_path)
            .field("resolution", &self.resolution)
            .finish_non_exhaustive()
    }
}

impl PreparedWriteTarget {
    /// Final relative path the staged file will be renamed to on commit.
    pub fn target_relative_path(&self) -> &Path {
        &self.final_relative_path
    }

    /// How the conflict policy was resolved against the live filesystem.
    pub fn resolution(&self) -> ConflictResolution {
        self.resolution
    }

    /// Borrow the staged file for reads (e.g. computing a digest).
    ///
    /// The handle is only available before [`commit`](Self::commit) or
    /// [`rollback`](Self::rollback); both close it so the Windows publish can
    /// rename the staged path.
    pub fn staged_file(&self) -> &File {
        self.staged_file
            .as_ref()
            .expect("staged file handle taken by commit or rollback")
    }

    /// Append `bytes` to the staged file.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.authority.validate()?;
        let staged_file = self
            .staged_file
            .as_mut()
            .expect("staged file handle taken by commit or rollback");
        staged_file.write_all(bytes)?;
        staged_file.flush()?;
        self.authority.validate()?;
        Ok(())
    }

    /// Close the retained staged-file handle before publish or rollback.
    ///
    /// Windows: `MoveFileExW` needs DELETE access on the source path, and
    /// `DeleteFileW` the same through share modes; the staged handle is
    /// opened without `FILE_SHARE_DELETE`, so any live handle on the file
    /// makes both fail with a sharing violation (os error 32). Dropping our
    /// own handle first is platform-neutral: POSIX rename/unlink never cared
    /// about open handles.
    fn close_staged_handle(&mut self) {
        drop(self.staged_file.take());
    }

    /// Commit the staged file atomically to its destination.
    ///
    /// On Unix this is a single `rename(2)`; on Windows a `MoveFileExW`
    /// replacement. The staged handle is closed first (see
    /// [`Self::close_staged_handle`]), and the mount boundary is revalidated
    /// immediately before and after the rename so a binder swap or remount
    /// between staging and commit cannot authorise a partial publish.
    pub fn commit(mut self) -> io::Result<CommitOutcome> {
        let final_path = self.authority.root().join(&self.final_relative_path);
        self.authority.validate()?;
        self.close_staged_handle();
        publish_atomic(&self.staged_path, &final_path)?;
        self.authority.validate()?;
        self.committed = true;
        Ok(CommitOutcome {
            relative_path: self.final_relative_path.clone(),
            resolution: self.resolution,
        })
    }

    /// Discard the staged file and any partial writes.
    pub fn rollback(mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        self.close_staged_handle();
        let outcome = rollback_staged(&self.staged_path);
        let _ = self.authority.validate();
        outcome
    }
}

impl Drop for PreparedWriteTarget {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Best-effort cleanup if the caller forgets to roll back explicitly.
        self.close_staged_handle();
        let _ = rollback_staged(&self.staged_path);
    }
}

/// Detail of what `commit` actually published, for callers that need to log
/// or report the publish outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    /// The relative path beneath the retained mount that now names the data.
    pub relative_path: PathBuf,
    /// How the conflict policy was resolved against the live filesystem.
    pub resolution: ConflictResolution,
}

/// Retained write authority over one exact mounted filesystem.
///
/// The underlying [`MountedRootAuthority`] is shared so the read-side scans
/// and the write-side commits always observe the same mount generation and
/// boundary. A successful transfer followed by a remount is detected on the
/// next `validate()` and produces a fail-closed error rather than attempting
/// a partial commit.
#[derive(Clone)]
pub struct MountedWriteAuthority {
    mounted: Arc<MountedRootAuthority>,
}

impl MountedWriteAuthority {
    /// Wrap an existing mounted authority to expose write API.
    pub fn from_mounted(mounted: Arc<MountedRootAuthority>) -> Self {
        Self { mounted }
    }

    /// Acquire a fresh write authority on the absolute mounted path.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let mounted = MountedRootAuthority::acquire(root)?;
        Ok(Self {
            mounted: Arc::new(mounted),
        })
    }

    /// The exact native mount path retained by this authority.
    pub fn root(&self) -> &Path {
        self.mounted.root()
    }

    /// Return the wrapped read authority for read operations.
    pub fn mount(&self) -> &Arc<MountedRootAuthority> {
        &self.mounted
    }

    /// Reverify the mount is still current.
    pub fn validate(&self) -> io::Result<()> {
        self.mounted.validate()
    }

    /// Prepare a writable target below the root. The destination path is
    /// checked against the conflict policy; a fresh, sibling temp file is
    /// created with `O_CREAT | O_EXCL` so a concurrent writer cannot smuggle
    /// a same-named file past publish.
    pub fn prepare_write_relative_file(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<PreparedWriteTarget> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_relative = assemble_relative(&components);
        let final_path = self.mounted.root().join(&final_relative);

        // The destination parent directory must be opened through the
        // retained authority so the boundary check matches the read path.
        // The bound handle is kept alive and used to create the staged
        // file (`openat` on Unix) so the parent cannot be swapped by a
        // symlink or bind between validation and creation.
        let parent_components = parent_components_of(&components);
        let parent_bound = if parent_components.as_os_str().is_empty() {
            self.mounted.bind_root_directory()?
        } else {
            self.mounted.open_relative_directory(&parent_components)?
        };
        let parent_handle = parent_bound.try_clone_file()?;
        drop(parent_bound);

        let (resolution, final_relative, staged_dir) = resolve_prepare_policy(
            self.mounted.root(),
            policy,
            &final_path,
            final_relative,
            &parent_components,
            components.last().expect("non-empty"),
        )?;

        let staged_name = staging_leaf_name();
        let staged_path_abs = self.mounted.root().join(&staged_dir).join(&staged_name);
        let staged_file = create_exclusive_staged_file(&staged_path_abs, &parent_handle)?;
        self.mounted.validate()?;

        Ok(PreparedWriteTarget {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            final_relative_path: final_relative,
            staged_path: staged_path_abs,
            staged_file: Some(staged_file),
            resolution,
            committed: false,
        })
    }

    /// Create a directory beneath the mount and bind it for further writes.
    pub fn create_relative_directory(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<MountedDirectory> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));

        match std::fs::symlink_metadata(&final_path) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "path exists and is not a directory",
                    ));
                }
                match policy {
                    ConflictPolicy::Skip | ConflictPolicy::Fail => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "directory exists and policy forbids overwriting",
                        ));
                    }
                    ConflictPolicy::Overwrite | ConflictPolicy::Preserve => {}
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_directory_atomic(self.mounted.root(), &components)?;
            }
            Err(error) => return Err(error),
        }
        self.mounted.validate()?;
        let _bound = self
            .mounted
            .open_relative_directory(&assemble_relative(&components))?;
        Ok(MountedDirectory {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            relative_path: assemble_relative(&components),
        })
    }

    /// Remove a regular file atomically through the retained authority.
    pub fn remove_relative_file(&self, relative: &Path) -> io::Result<()> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));
        let metadata = std::fs::symlink_metadata(&final_path)?;
        if metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove a directory through remove_relative_file",
            ));
        }
        std::fs::remove_file(&final_path)?;
        self.mounted.validate()?;
        Ok(())
    }

    /// Remove an empty directory atomically through the retained authority.
    pub fn remove_relative_directory(&self, relative: &Path) -> io::Result<()> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));
        let metadata = std::fs::symlink_metadata(&final_path)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove a non-directory through remove_relative_directory",
            ));
        }
        std::fs::remove_dir(&final_path)?;
        self.mounted.validate()?;
        Ok(())
    }
}

/// A directory created by [`MountedWriteAuthority::create_relative_directory`].
pub struct MountedDirectory {
    lease_token: Uuid,
    authority: Arc<MountedRootAuthority>,
    relative_path: PathBuf,
}

impl MountedDirectory {
    /// Return the relative path of this directory.
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// Prepare a writable file directly inside this directory.
    pub fn prepare_write_in_directory(
        &self,
        name: &str,
        policy: ConflictPolicy,
    ) -> io::Result<PreparedWriteTarget> {
        let mut relative = self.relative_path.clone();
        relative.push(name);
        MountedWriteAuthority::from_mounted(Arc::clone(&self.authority))
            .prepare_write_relative_file(&relative, policy)
    }
}

mod staging;
#[cfg(test)]
mod tests;

use staging::{
    assemble_relative, create_directory_atomic, create_exclusive_staged_file, parent_components_of,
    preserved_sibling_path, publish_atomic, rollback_staged, staging_leaf_name,
    strict_relative_components,
};
