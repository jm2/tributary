//! The staged-write handle ([`PreparedWriteTarget`]) and the bound
//! directory handle ([`MountedDirectory`]) produced by the write authority.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::policy::{CommitOutcome, ConflictPolicy, ConflictResolution};
#[cfg(unix)]
use super::staging::discard_staged_file;
#[cfg(not(unix))]
use super::staging::{discard_staged_file, rollback_staged};
use crate::local::root_authority::{MountedRootAuthority, RetainedWriteParent};

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
    /// The destination parent directory retained from staging through
    /// commit. Publication is anchored to this exact object so a parent or
    /// mount replacement between staging and commit cannot redirect the
    /// write.
    pub(super) parent: RetainedWriteParent,
    pub(super) final_relative_path: PathBuf,
    /// Leaf name of the staged temporary file inside the retained parent.
    pub(super) staged_leaf: OsString,
    /// Absolute path of the staged temporary file. Sibling of the destination
    /// so the rename is atomic on the same filesystem; Windows publishes and
    /// discards through this path after the retained parent is revalidated.
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
    /// On Unix this is a single `renameat(2)` anchored to the parent
    /// directory handle retained at staging time, so a parent or mount
    /// replacement between staging and commit cannot redirect the publish.
    /// Fresh and Preserved resolutions publish with no-replace semantics: a
    /// destination that appeared after staging — including the original file
    /// of a Preserve conflict — fails the commit instead of being replaced,
    /// and is never touched.
    ///
    /// An Overwrite resolution publishes by replace, but never unbacked:
    /// the occupant of the destination name is bound to a hidden backup
    /// sibling at commit time (a hard link where the filesystem supports
    /// them, else a verified commit-time copy), and the backup's relative
    /// path is reported on the outcome as `replaced_original` so the caller
    /// can restore exactly the bytes that were destroyed. If the
    /// destination turned out to be absent at commit, the publish degrades
    /// to the no-replace cascade — a concurrent creation is bypassed or
    /// backed up, never silently replaced-and-deleted. A directory occupant
    /// is refused with a typed `InvalidInput` error.
    ///
    /// On Windows a `MoveFileExW` publish is bracketed by retained-parent
    /// identity revalidations. The staged handle is flushed to disk and
    /// closed before the rename: Windows refuses to rename or delete a file
    /// while a handle without `FILE_SHARE_DELETE` is open, and a publish
    /// must not depend on handle sharing modes anyway. The mount boundary
    /// is revalidated immediately before and after the rename so a binder
    /// swap or remount between staging and commit cannot authorise a
    /// partial publish.
    pub fn commit(mut self) -> io::Result<CommitOutcome> {
        self.authority.validate()?;
        self.parent.validate_with(&self.authority)?;
        let final_path = self.authority.root().join(&self.final_relative_path);
        let final_leaf = self
            .final_relative_path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "final path is missing a leaf")
            })?
            .to_os_string();
        self.flush_and_close_staged()?;
        let replaced_original = if self.resolution == ConflictResolution::Overwrite {
            self.publish_overwrite_with_backup(&final_leaf, &final_path)?
        } else {
            self.authority.rename_within_directory(
                &self.parent,
                &self.staged_leaf,
                &self.staged_path,
                &final_leaf,
                &final_path,
                true,
            )?;
            None
        };
        self.authority.validate()?;
        self.committed = true;
        Ok(CommitOutcome {
            relative_path: self.final_relative_path.clone(),
            resolution: self.resolution,
            replaced_original,
        })
    }

    /// Flush the staged handle to disk and close it before any publish
    /// step. Windows refuses to rename or delete a file while a handle
    /// without `FILE_SHARE_DELETE` is open, and a publish must not depend
    /// on handle sharing modes anywhere.
    fn flush_and_close_staged(&mut self) -> io::Result<()> {
        let staged_file = self
            .staged_file
            .take()
            .expect("staged handle is open until commit");
        staged_file.sync_all()?;
        drop(staged_file);
        Ok(())
    }

    /// Publish the staged file over an existing occupant by replace, never
    /// unbacked: the occupant of the destination name is bound to a hidden
    /// backup sibling at commit time (a hard link where the filesystem
    /// supports them, else a verified commit-time copy), and the backup's
    /// relative path is returned so the caller can restore exactly the
    /// bytes that were destroyed.
    fn publish_overwrite_with_backup(
        &self,
        final_leaf: &OsStr,
        final_path: &Path,
    ) -> io::Result<Option<PathBuf>> {
        let backup_leaf = backup_leaf_name();
        let mut backup_relative = self
            .final_relative_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        backup_relative.push(backup_leaf.as_os_str());
        let backup_absolute = self.authority.root().join(&backup_relative);
        let replaced = self.authority.replace_within_directory(
            &self.parent,
            &self.staged_leaf,
            &self.staged_path,
            final_leaf,
            final_path,
            backup_leaf.as_os_str(),
            &backup_absolute,
        )?;
        Ok(replaced.then_some(backup_relative))
    }

    /// Discard the staged file and any partial writes.
    pub fn rollback(mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        drop(self.staged_file.take());
        self.discard_staged()
    }

    /// Unlink the staged temporary file through the retained parent handle
    /// (Unix) or by its absolute path after revalidation (Windows). A lost
    /// authority fails closed: the staged file is never removed through a
    /// namespace that no longer names the audited mount.
    fn discard_staged(&self) -> io::Result<()> {
        if self.parent.validate_with(&self.authority).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authority is no longer current; leaving staged file untouched",
            ));
        }
        let outcome = discard_staged_file(
            Some(self.parent.handle()),
            &self.staged_leaf,
            &self.staged_path,
        );
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
        // A lost authority leaves the staged file untouched; the transfer's
        // error path surfaces the authority loss instead.
        if self.parent.validate_with(&self.authority).is_ok() {
            #[cfg(unix)]
            {
                use rustix::fs::{unlinkat, AtFlags};

                let _ = unlinkat(self.parent.handle(), &self.staged_leaf, AtFlags::empty());
            }
            #[cfg(not(unix))]
            {
                let _ = rollback_staged(&self.staged_path);
            }
        }
    }
}

/// A unique hidden leaf name for a commit-time backup sibling: the
/// occupant destroyed by an Overwrite publish is bound here so a rollback
/// can restore it, and a successful transfer discards it.
fn backup_leaf_name() -> OsString {
    let mut name = OsString::from(".tributary-backup-");
    name.push(Uuid::new_v4().to_string());
    name.push(".tmp");
    name
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
