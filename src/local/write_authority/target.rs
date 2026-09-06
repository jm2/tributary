//! The staged-write handle ([`PreparedWriteTarget`]) and the bound
//! directory handle ([`MountedDirectory`]) produced by the write authority.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::policy::{CommitOutcome, ConflictPolicy, ConflictResolution};
use super::staging::{
    backup_leaf_name, bind_publish_parent, rename_bounded, rollback_staged, BoundParent,
};
use crate::local::root_authority::MountedRootAuthority;

/// The parent-relative prefix of a strict relative path (`""` for a file
/// directly beneath the mount root).
pub(super) fn parent_relative_of(relative: &Path) -> PathBuf {
    match relative.parent() {
        Some(parent) => parent.to_path_buf(),
        None => PathBuf::new(),
    }
}

/// The leaf name of a path, for descriptor-relative renames.
pub(super) fn leaf_of(relative: &Path) -> io::Result<std::ffi::OsString> {
    relative.file_name().map(|leaf| leaf.to_os_string()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write target path is missing a leaf",
        )
    })
}

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
    /// Publish with atomic no-replace semantics: an existing destination
    /// makes the commit fail with `AlreadyExists` instead of replacing it.
    /// Set for Fresh and Preserved resolutions; the Overwrite resolution
    /// replaces (after moving the original aside) so an interrupted transfer
    /// can always restore what it displaced.
    pub(super) no_replace: bool,
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

    /// Commit the staged file to its destination through the retained
    /// authority.
    ///
    /// The parent directory is bound through the retained-root machinery and
    /// the publish rename is issued against that binding, so a swap between
    /// validation and publish cannot redirect the write (no
    /// validate-then-path-rename window).
    ///
    /// Fresh and Preserved resolutions publish with atomic no-replace
    /// semantics: a destination that appeared after the plan fails the commit
    /// with `AlreadyExists` instead of being replaced. The Overwrite
    /// resolution first moves a pre-existing destination aside to a unique
    /// backup name, records it on the [`CommitOutcome`], and restores it
    /// before surfacing any publish failure, so the mount never loses the
    /// original even when the publish itself fails.
    ///
    /// The staged handle is flushed to disk and closed before the rename:
    /// Windows refuses to rename or delete a file while a handle without
    /// `FILE_SHARE_DELETE` is open, and a publish must not depend on handle
    /// sharing modes anyway. The mount boundary is revalidated immediately
    /// before and after the rename so a binder swap or remount cannot
    /// authorise a partial publish.
    pub fn commit(mut self) -> io::Result<CommitOutcome> {
        self.authority.validate()?;
        let staged_file = self
            .staged_file
            .take()
            .expect("staged handle is open until commit");
        staged_file.sync_all()?;
        drop(staged_file);

        let parent_relative = parent_relative_of(&self.final_relative_path);
        let parent: BoundParent = bind_publish_parent(&self.authority, &parent_relative)?;
        let final_leaf = leaf_of(&self.final_relative_path)?;
        let staged_leaf = leaf_of(&self.staged_path)?;

        // Overwrite: move the original aside so a later rollback can restore
        // it. A directory destination is refused outright: this authority
        // publishes regular files, never directory trees.
        let backup_relative = if self.resolution == ConflictResolution::Overwrite {
            match std::fs::symlink_metadata(self.authority.root().join(&self.final_relative_path)) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "destination is a directory; refusing to overwrite it with a file",
                    ));
                }
                Ok(_) => {
                    let backup_leaf = backup_leaf_name();
                    rename_bounded(&parent, &final_leaf, &backup_leaf, false)?;
                    Some(parent_relative.join(&backup_leaf))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };

        let publish =
            rename_bounded(&parent, &staged_leaf, &final_leaf, self.no_replace);
        if let Err(error) = publish {
            // Never leave the mount without its original: put a moved-aside
            // original back before surfacing the failure.
            if let Some(backup) = &backup_relative {
                let backup_leaf = leaf_of(backup)?;
                if let Err(restore_error) =
                    rename_bounded(&parent, &backup_leaf, &final_leaf, false)
                {
                    return Err(io::Error::other(format!(
                        "publish failed ({error}) and restoring the saved original also failed ({restore_error})"
                    )));
                }
            }
            return Err(error);
        }
        self.authority.validate()?;
        self.committed = true;
        Ok(CommitOutcome {
            relative_path: self.final_relative_path.clone(),
            resolution: self.resolution,
            backup_relative_path: backup_relative,
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
