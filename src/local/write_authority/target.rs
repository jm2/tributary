//! The staged-write handle ([`PreparedWriteTarget`]) and the bound
//! directory handle ([`MountedDirectory`]) produced by the write authority.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::policy::{CommitOutcome, ConflictPolicy, ConflictResolution};
use super::staging::{publish_atomic, rollback_staged};
use crate::local::root_authority::MountedRootAuthority;

/// A staged write below a [`MountedWriteAuthority`](super::MountedWriteAuthority)
/// ready for commit/rollback.
///
/// The destination is held in a sibling temporary file. Until
/// [`commit`](Self::commit) is called the original destination is untouched,
/// so a partially-written staged file can be discarded without disturbing the
/// mount. Once commit fires, the rename is atomic on the same filesystem and
/// the staged file is gone.
pub struct PreparedWriteTarget {
    pub(super) lease_token: Uuid,
    pub(super) authority: Arc<MountedRootAuthority>,
    pub(super) final_relative_path: PathBuf,
    /// Absolute path of the staged temporary file. Sibling of the destination
    /// so the rename is atomic on the same filesystem.
    pub(super) staged_path: PathBuf,
    /// Handle on the staged temporary file. Taken and closed before the
    /// staged path is renamed or removed: on Windows a rename/delete of a
    /// file fails with a sharing violation while any handle opened without
    /// `FILE_SHARE_DELETE` is still open, so the handle must never outlive
    /// the write phase. `None` only inside commit/rollback/drop.
    pub(super) staged_file: Option<File>,
    pub(super) resolution: ConflictResolution,
    pub(super) committed: bool,
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

    /// Absolute path of the staged temporary file backing this target.
    pub fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    /// Borrow the staged file for reads (e.g. computing a digest).
    ///
    /// The handle is closed when [`commit`](Self::commit) or
    /// [`rollback`](Self::rollback) runs; a borrow must not outlive either.
    pub fn staged_file(&self) -> &File {
        self.staged_file
            .as_ref()
            .expect("staged handle is open until commit or rollback")
    }

    /// Append `bytes` to the staged file.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.authority.validate()?;
        let staged = self
            .staged_file
            .as_mut()
            .expect("staged handle is open until commit or rollback");
        staged.write_all(bytes)?;
        staged.flush()?;
        self.authority.validate()?;
        Ok(())
    }

    /// Commit the staged file atomically to its destination.
    ///
    /// On Unix this is a single `rename(2)`; on Windows a `MoveFileExW`
    /// replacement. The staged handle is flushed to disk and closed before
    /// the rename: Windows refuses to rename or delete a file while a
    /// handle without `FILE_SHARE_DELETE` is open, and a publish must not
    /// depend on handle sharing modes anyway. The mount boundary is
    /// revalidated immediately before and after the rename so a binder swap
    /// or remount between staging and commit cannot authorise a partial
    /// publish.
    pub fn commit(mut self) -> io::Result<CommitOutcome> {
        let final_path = self.authority.root().join(&self.final_relative_path);
        self.authority.validate()?;
        let staged_file = self
            .staged_file
            .take()
            .expect("staged handle is open until commit");
        staged_file.sync_all()?;
        drop(staged_file);
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
        drop(self.staged_file.take());
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
        // Close the staged handle before removal: Windows refuses to delete
        // a file while a handle without FILE_SHARE_DELETE is open.
        drop(self.staged_file.take());
        // Best-effort cleanup if the caller forgets to roll back explicitly.
        let _ = rollback_staged(&self.staged_path);
    }
}

/// A directory created by
/// [`MountedWriteAuthority::create_relative_directory`](super::MountedWriteAuthority::create_relative_directory).
pub struct MountedDirectory {
    pub(super) lease_token: Uuid,
    pub(super) authority: Arc<MountedRootAuthority>,
    pub(super) relative_path: PathBuf,
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
        super::MountedWriteAuthority::from_mounted(Arc::clone(&self.authority))
            .prepare_write_relative_file(&relative, policy)
    }
}
