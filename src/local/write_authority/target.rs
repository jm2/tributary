//! The staged-write handle ([`PreparedWriteTarget`]) and the bound
//! directory handle ([`MountedDirectory`]) produced by the write authority.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::policy::{
    CommitDisposition, CommitError, CommitOutcome, ConflictPolicy, ConflictResolution,
};
#[cfg(unix)]
use super::staging::discard_staged_file;
#[cfg(not(unix))]
use super::staging::{discard_staged_file, rollback_staged};
use crate::local::root_authority::{
    BoundOccupantBackup, LandedPublish, LeafIdentity, MountedRootAuthority, RetainedWriteParent,
};
// The displaced-occupant marker exists only where the atomic exchange does
// (Unix; see `root_authority::RestoreFailure`), and the exhausted-rebind
// marker only on Windows (see `root_authority::ExhaustedRebindBackup`):
// both carry state a plain I/O error would strand, so the commit-time
// downcasts below are gated per platform.
#[cfg(windows)]
use crate::local::root_authority::ExhaustedRebindBackup;
#[cfg(unix)]
use crate::local::root_authority::RestoreFailure;

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
    /// Set when an Overwrite publish landed its atomic exchange but could
    /// not restore the displaced occupant it captured: the transfer's bytes
    /// ARE published at the destination, the bind-time backup is retained
    /// for restoration, and the displaced object survives at the staged
    /// leaf. The staged leaf must be shielded from every cleanup path —
    /// unlinking it would destroy the displaced object's last reachable
    /// link — and the caller recovers the published outcome from the
    /// `PublishVerification` error to record it for rollback.
    ///
    /// Held as a one-shot latch in a `Cell` so the publish path arms it
    /// behind `&self`: the unix-only marker path that sets it compiles out
    /// on Windows, where no publish step mutates the target.
    pub(super) staged_leaf_holds_displaced_occupant: Cell<bool>,
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
    ///
    /// Errors are typed: [`CommitError::Io`] means nothing was published
    /// and nothing was displaced, while
    /// [`CommitError::PublishVerification`] means the destination's state
    /// changed and MUST be recorded before the failure surfaces — either
    /// the staged bytes WERE published but a post-publish verification
    /// failed (the retained-parent revalidation reported by the publish
    /// step itself, or the trailing authority revalidation here), or a
    /// Windows replace publish exhausted its rebind bound with a completed
    /// binding retained: nothing landed, but the displaced original
    /// survives at its verified backup. The latter outcome carries
    /// [`CommitDisposition::DisplacedOnly`], so the caller records the
    /// retained backup for a restore that requires an absent slot instead
    /// of reversing it as an identity-less replacement. A committed or
    /// displaced state dropped as a plain I/O error can never be undone or
    /// disposed.
    // The verified-publication payload grew by the replaced occupant's
    // bind-time identity: the rollback couples the retained backup to that
    // exact object, and boxing the payload to appease the size lint would
    // obscure the every-`?`-carries-the-outcome contract.
    #[allow(clippy::result_large_err)]
    pub fn commit(mut self) -> Result<CommitOutcome, CommitError> {
        self.authority.validate().map_err(CommitError::from)?;
        self.parent
            .validate_with(&self.authority)
            .map_err(CommitError::from)?;
        self.flush_and_close_staged().map_err(CommitError::from)?;
        let (replaced, landed) = self.publish_staged()?;
        let outcome = CommitOutcome {
            relative_path: self.final_relative_path.clone(),
            resolution: self.resolution,
            disposition: CommitDisposition::Published,
            replaced_original: replaced.as_ref().map(|backup| backup.relative_path.clone()),
            replaced_original_leaf: replaced.and_then(|backup| backup.leaf),
            published_leaf: landed.published_leaf,
        };
        // The bytes are now at the destination no matter what happens next:
        // a failed post-publish verification must not discard the outcome,
        // or the published file (and a replaced occupant's saved backup)
        // would be unrecorded and unreachable from rollback. This includes
        // the trailing retained-parent revalidation reported by the publish
        // step itself: the rename landed, so that failure is a verification
        // failure about an existing publication, never an unpublished I/O
        // failure.
        if let Err(error) = landed
            .post_validate
            .and_then(|()| self.authority.validate())
        {
            return Err(CommitError::PublishVerification { outcome, error });
        }
        self.committed = true;
        Ok(outcome)
    }

    /// Publish the flushed-and-closed staged leaf under the planned
    /// resolution.
    ///
    /// Returns the bound backup of the replaced occupant — `Some` only
    /// when an Overwrite publish actually replaced an occupant — plus the
    /// landing data whose trailing retained-parent revalidation the caller
    /// reports as a verified-publication failure.
    #[allow(clippy::result_large_err)]
    fn publish_staged(&self) -> Result<(Option<BoundOccupantBackup>, LandedPublish), CommitError> {
        let final_path = self.authority.root().join(&self.final_relative_path);
        let final_leaf = self.final_leaf_name().map_err(CommitError::from)?;
        if self.resolution != ConflictResolution::Overwrite {
            // Fresh and Preserved resolutions publish with no-replace
            // semantics: a destination that appeared after staging —
            // including the original file of a Preserve conflict — fails
            // the commit instead of being replaced, and is never touched.
            let landed = self
                .authority
                .publish_within_directory(
                    &self.parent,
                    &self.staged_leaf,
                    &self.staged_path,
                    &final_leaf,
                    &final_path,
                    true,
                )
                .map_err(CommitError::from)?;
            return Ok((None, landed));
        }
        self.publish_by_replace(final_leaf, final_path)
    }

    /// The Overwrite publish: bind the current occupant of the destination
    /// name to a hidden backup sibling and replace atomically — through
    /// object-coupled primitives, never an unbacked replace. If the
    /// destination turned out to be absent at commit, the publish degrades
    /// to the no-replace cascade — a concurrent creation is bypassed or
    /// backed up, never silently replaced-and-deleted.
    ///
    /// The backup leaf is minted privately; the replace loop mints a fresh
    /// private name for every re-bind (a Windows bind whose publish
    /// collided keeps its completed binding, so the loop never reuses a
    /// name that may hold a displaced object).
    #[allow(clippy::result_large_err)]
    fn publish_by_replace(
        &self,
        final_leaf: OsString,
        final_path: PathBuf,
    ) -> Result<(Option<BoundOccupantBackup>, LandedPublish), CommitError> {
        let backup_leaf = backup_leaf_name();
        let backup_relative = self.backup_relative_path(&backup_leaf);
        let initial_absolute = self.authority.root().join(&backup_relative);
        let mut rebinding_backup = || {
            let leaf = backup_leaf_name();
            let relative = self.backup_relative_path(&leaf);
            (leaf, relative)
        };
        match self.authority.replace_within_directory(
            &self.parent,
            &self.staged_leaf,
            &self.staged_path,
            &final_leaf,
            &final_path,
            backup_leaf.as_os_str(),
            &initial_absolute,
            &backup_relative,
            &mut rebinding_backup,
        ) {
            Ok((replaced, landed, backup)) => Ok((backup.filter(|_| replaced), landed)),
            Err(error) => {
                // Windows: an exhausted rebind bound with a completed
                // binding retained published nothing, but the displaced
                // original survives at its verified backup — carried by the
                // error payload. Fold it into a verified-publication
                // failure carrying the outcome so the caller records the
                // replacement for rollback; surfacing it as a plain I/O
                // error would strand the backup as an unrecorded hidden
                // orphan.
                #[cfg(windows)]
                if let Some(exhausted) = error
                    .get_ref()
                    .and_then(|payload| payload.downcast_ref::<ExhaustedRebindBackup>())
                {
                    let outcome = CommitOutcome {
                        relative_path: self.final_relative_path.clone(),
                        resolution: self.resolution,
                        // Nothing of the transfer's landed: the destination
                        // still holds a concurrent occupant (or is vacant),
                        // and only the pre-transfer occupant's backup was
                        // retained. Recording this as `Published` with an
                        // identity-less `published_leaf` would let rollback
                        // reverse it as a replacement and delete a
                        // concurrent writer's file; `DisplacedOnly` requires
                        // an absent slot instead.
                        disposition: CommitDisposition::DisplacedOnly,
                        replaced_original: Some(exhausted.backup_relative.clone()),
                        replaced_original_leaf: Some(exhausted.backup_leaf),
                        published_leaf: None,
                    };
                    return Err(CommitError::PublishVerification { outcome, error });
                }
                #[cfg(not(unix))]
                return Err(CommitError::from(error));
                #[cfg(unix)]
                Err(self.map_replace_failure(error, backup_relative))
            }
        }
    }

    /// The hidden backup sibling path for a backup leaf: same parent
    /// directory as the destination, so the swap and the later restore
    /// stay within one retained parent.
    fn backup_relative_path(&self, backup_leaf: &OsStr) -> PathBuf {
        let mut backup_relative = self
            .final_relative_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        backup_relative.push(backup_leaf);
        backup_relative
    }

    /// Classify a failed replace publish. A landed atomic exchange whose
    /// displaced-occupant restore failed means the transfer's bytes ARE
    /// published at the destination, the bind-time backup is retained for
    /// restoration, and the displaced object survives at the staged leaf:
    /// shield the staged leaf from cleanup and surface the publication as a
    /// verified-publication failure carrying the outcome, so the caller
    /// records it for rollback — a committed file whose outcome is dropped
    /// can never be undone. Any other failure published nothing.
    ///
    /// Unix only: the atomic exchange — and so the landed-but-unrestorable
    /// displaced occupant — exists only on platforms with a swap primitive.
    /// The Windows exhausted-rebind disposition is downcast in
    /// [`Self::publish_by_replace`] instead: nothing was published there,
    /// but a completed binding's retained backup travels with the error.
    #[cfg(unix)]
    fn map_replace_failure(&self, error: io::Error, backup_relative: PathBuf) -> CommitError {
        let displaced = error
            .get_ref()
            .and_then(|payload| payload.downcast_ref::<RestoreFailure>())
            .map(|failure| (failure.payload.published_leaf, failure.payload.backup_leaf));
        if let Some((published_leaf, backup_leaf)) = displaced {
            self.staged_leaf_holds_displaced_occupant.set(true);
            let outcome = CommitOutcome {
                relative_path: self.final_relative_path.clone(),
                resolution: self.resolution,
                disposition: CommitDisposition::Published,
                replaced_original: Some(backup_relative),
                replaced_original_leaf: backup_leaf,
                published_leaf,
            };
            return CommitError::PublishVerification { outcome, error };
        }
        CommitError::from(error)
    }

    /// The leaf name the staged bytes publish under.
    fn final_leaf_name(&self) -> io::Result<OsString> {
        self.final_relative_path
            .file_name()
            .map(OsStr::to_os_string)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "final path is missing a leaf")
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

    /// Discard the staged file and any partial writes. A displaced-occupant
    /// failure leaves the staged leaf deliberately in place: it preserves
    /// the object the publish displaced, and unlinking it would destroy a
    /// concurrent writer's (or the original occupant's) last reachable
    /// link.
    pub fn rollback(mut self) -> io::Result<()> {
        if self.committed || self.staged_leaf_holds_displaced_occupant.get() {
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
        if self.committed || self.staged_leaf_holds_displaced_occupant.get() {
            // A displaced-occupant failure keeps the staged leaf: it holds
            // the displaced object's last reachable link, not staged
            // garbage. See `staged_leaf_holds_displaced_occupant`.
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
    /// Best-effort no-follow identity of the created directory, captured at
    /// bind time for identity-verified rollback. `None` when the identity
    /// could not be captured; such directories degrade to the legacy
    /// path-only reversal.
    pub(super) identity: Option<LeafIdentity>,
}

impl MountedDirectory {
    /// Return the relative path of this directory.
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// The no-follow identity captured for this directory at creation, if
    /// the platform could capture one. Rollback compares this against the
    /// directory before removing it.
    pub fn identity(&self) -> Option<LeafIdentity> {
        self.identity
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
