//! The transfer executor's rollback half.
//!
//! Outcome-driven reversal of every owned destination change, split out of
//! the executor proper: rollback walks the committed changes in reverse
//! order, revalidates the destination authority before each reversal, and
//! refuses — rather than touches — any leaf a concurrent writer replaced
//! after the transfer published it.

use std::io;
use std::path::{Path, PathBuf};

use super::super::types::TransferError;
use super::{RunContext, TransferExecutor};
use crate::local::root_authority::LeafIdentity;
use crate::local::write_authority::{
    CommitOutcome, ConflictResolution, PreparedWriteTarget, ReversalOutcome,
};

/// One destination mutation the executor owns and must undo on rollback.
///
/// Rollback reverses what was actually published, never what the plan
/// predicted. A Preserve conflict publishes to a renamed sibling — removing
/// the planned path would destroy the pre-existing original — and an
/// Overwrite commit replaces an original that only a saved copy can restore.
///
/// Every variant also carries the no-follow leaf identity the write
/// authority captured at publish/creation time. Before each reversal
/// mutation the destination is re-stat'ed and the identity compared: a
/// mismatch means a concurrent writer replaced the transfer's publication
/// after it committed, so reversing by pathname alone would delete — or
/// restore a backup over — a file the transfer does not own. Such a leaf is
/// refused and the rollback fails instead. `None` degrades that reversal to
/// the legacy path-only behavior (identity capture is best-effort).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum OwnedChange {
    /// A file the executor published at this path; rollback removes exactly
    /// this path. Fresh publishes and preserved siblings both land here.
    PublishedFile {
        relative_path: PathBuf,
        published_leaf: Option<LeafIdentity>,
    },
    /// A pre-existing file the executor replaced; its bytes were saved at
    /// `backup_relative_path` before the replace. Rollback republishes
    /// the backup over `relative_path` and consumes the backup. The
    /// backup's bind-time identity (`backup_leaf`) is verified —
    /// change-instant-exact, so a same-index swap-in is refused — before
    /// BOTH the restoration and the
    /// successful-transfer disposal: a backup name a concurrent writer
    /// swapped for a foreign object is refused fail-closed, never
    /// installed as the original and never silently deleted.
    ReplacedFile {
        relative_path: PathBuf,
        backup_relative_path: PathBuf,
        backup_leaf: Option<LeafIdentity>,
        published_leaf: Option<LeafIdentity>,
    },
    /// A directory (or directory ancestor) the executor created; it must be
    /// empty once every file inside it has been rolled back. Recorded only
    /// when the directory was provably absent immediately before creation.
    CreatedDirectory {
        relative_path: PathBuf,
        created_directory: Option<LeafIdentity>,
    },
}

/// Classify a committed copy into the owned change rollback must reverse.
///
/// The commit outcome is authoritative about what the publish actually did:
/// an Overwrite commit that bound a replaced original (`replaced_original`)
/// is rolled back by restoring that backup, while an Overwrite commit that
/// replaced nothing (the occupant vanished before the publish), a fresh
/// publish, and a preserved sibling all roll back by removing the actual
/// published path.
pub(super) fn owned_change_for_copy(outcome: CommitOutcome) -> OwnedChange {
    match outcome.resolution {
        ConflictResolution::Overwrite => match outcome.replaced_original {
            Some(backup_relative_path) => OwnedChange::ReplacedFile {
                relative_path: outcome.relative_path,
                backup_relative_path,
                backup_leaf: outcome.replaced_original_leaf,
                published_leaf: outcome.published_leaf,
            },
            None => OwnedChange::PublishedFile {
                relative_path: outcome.relative_path,
                published_leaf: outcome.published_leaf,
            },
        },
        ConflictResolution::Fresh | ConflictResolution::Preserved => OwnedChange::PublishedFile {
            relative_path: outcome.relative_path,
            published_leaf: outcome.published_leaf,
        },
    }
}

impl TransferExecutor {
    /// Remove the saved originals of successful overwrite commits.
    ///
    /// A completed transfer supersedes every saved original: each backup is
    /// a full-size hidden sibling that would otherwise accumulate until
    /// destination storage is exhausted. Removal is best-effort about
    /// ORDINARY failures — a leftover hidden backup is preferable to
    /// failing a transfer whose bytes are already published, matching the
    /// link-based publish's policy for its staged-leaf unlink — but never
    /// about IDENTITY: each backup is verified against the bind-time
    /// identity recorded when the overwrite commit bound it. A backup name
    /// a concurrent writer swapped for a foreign object is refused: the
    /// foreign object is neither installed as the original nor silently
    /// deleted, and the refusal surfaces as a transfer failure instead of
    /// a silently successful summary. The backups also remain restorable
    /// on the failure path, which never reaches this method.
    pub(super) fn discard_superseded_backups(
        &self,
        context: &RunContext<'_>,
    ) -> Result<(), TransferError> {
        for change in &context.committed {
            if let OwnedChange::ReplacedFile {
                backup_relative_path,
                backup_leaf,
                ..
            } = change
            {
                match self
                    .request
                    .destination
                    .discard_backup_relative_file(backup_relative_path, *backup_leaf)
                {
                    Ok(ReversalOutcome::Reversed | ReversalOutcome::AlreadyAbsent) => {}
                    Ok(ReversalOutcome::RefusedForeignLeaf) => {
                        return Err(TransferError::RollbackFailed {
                            path: backup_relative_path.clone(),
                            context: "the saved overwrite backup was replaced by a concurrent \
                                      writer after the transfer committed; refusing to discard \
                                      an object the transfer does not own"
                                .to_string(),
                        });
                    }
                    Err(error) => {
                        return Err(TransferError::RollbackFailed {
                            path: backup_relative_path.clone(),
                            context: format!(
                                "the saved overwrite backup could not be discarded: {error}"
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Reverse every owned change in reverse commit order, revalidating the
    /// destination authority before each reversal.
    ///
    /// Published files are removed exactly where they were published — a
    /// preserved sibling is removed by its own name, never by the planned
    /// destination. Replaced files are restored from their saved backups
    /// (the backup itself verified against its bind-time identity).
    /// Created directories are removed deepest-first after their files are
    /// gone; an unexpected residue fails the rollback rather than deleting
    /// data the executor does not own.
    ///
    /// Every reversal is identity-verified first: the leaf recorded at
    /// publish/creation time is compared against whatever currently
    /// occupies the path. A concurrent writer that replaced a published
    /// destination after its stage completed but before a later stage
    /// failed must never have its file deleted or restored over by this
    /// rollback — such a leaf is refused and the reversal fails.
    ///
    /// A refused or failed reversal does NOT abandon the remaining
    /// changes: the first failure is retained while every remaining
    /// independent change is still reversed (each identity-verified and
    /// authority-revalidated in turn), and the retained error surfaces
    /// after the loop with the path that could not be reversed. Leaving
    /// earlier committed changes in place because a later-committed one
    /// was refused would contradict the all-stages rollback contract and
    /// strand avoidable destination changes. An authority that is no
    /// longer current remains a stop-then-surface condition: the loop
    /// halts rather than touching a destination no retained root
    /// authorises, and the retained reversal failure — the more precise
    /// account of what could not be undone — is surfaced.
    pub(super) fn rollback(&self, context: &mut RunContext<'_>) -> Result<(), TransferError> {
        let mut first_failure: Option<TransferError> = None;
        while let Some(change) = context.committed.pop() {
            if let Err(error) = self.request.destination.validate() {
                return Err(first_failure.unwrap_or_else(|| {
                    TransferError::authority(format!("destination not current: {error}"))
                }));
            }
            let outcome = self.reverse_committed_change(change);
            if outcome.is_err() && first_failure.is_none() {
                first_failure = outcome.err();
            }
        }
        match first_failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Reverse one committed owned change: the recorded leaf identity is
    /// re-verified against whatever currently occupies the path before the
    /// reversal mutation, and a refused or failed reversal is surfaced
    /// instead of touching a leaf the transfer no longer owns.
    fn reverse_committed_change(&self, change: OwnedChange) -> Result<(), TransferError> {
        match change {
            OwnedChange::PublishedFile {
                relative_path,
                published_leaf,
            } => Self::reverse(
                &relative_path,
                self.request
                    .destination
                    .remove_relative_file_verified(&relative_path, published_leaf),
            ),
            OwnedChange::ReplacedFile {
                relative_path,
                backup_relative_path,
                backup_leaf,
                published_leaf,
            } => Self::reverse(
                &relative_path,
                self.request.destination.restore_relative_file_verified(
                    &backup_relative_path,
                    &relative_path,
                    published_leaf.as_ref(),
                    backup_leaf.as_ref(),
                ),
            ),
            OwnedChange::CreatedDirectory {
                relative_path,
                created_directory,
            } => Self::reverse(
                &relative_path,
                self.request
                    .destination
                    .remove_relative_directory_verified(&relative_path, created_directory),
            ),
        }
    }

    /// Map one reversal outcome into the rollback result. A refused
    /// reversal — the destination leaf was replaced by a concurrent writer
    /// after the transfer published it, or a saved backup no longer names
    /// the object it was bound to — fails the rollback instead of touching
    /// the foreign object.
    fn reverse(
        relative_path: &Path,
        outcome: io::Result<ReversalOutcome>,
    ) -> Result<(), TransferError> {
        match outcome {
            Ok(ReversalOutcome::Reversed | ReversalOutcome::AlreadyAbsent) => Ok(()),
            Ok(ReversalOutcome::RefusedForeignLeaf) => Err(TransferError::RollbackFailed {
                path: relative_path.to_path_buf(),
                context: "a leaf of the recorded change was replaced by a concurrent writer \
                          after the transfer committed it; refusing to reverse a file the \
                          transfer does not own"
                    .to_string(),
            }),
            Err(error) => Err(TransferError::RollbackFailed {
                path: relative_path.to_path_buf(),
                context: error.to_string(),
            }),
        }
    }
}

/// Map an I/O error carrying the cancellation interrupt into the typed
/// cancelled error, everything else into a transfer I/O error.
fn transfer_io(context: &'static str, error: io::Error) -> TransferError {
    if error.kind() == io::ErrorKind::Interrupted {
        TransferError::Cancelled
    } else {
        TransferError::io(context, error)
    }
}

/// Roll a staged file back and translate its copy-phase failure. The
/// staged sibling is discarded so a failed copy never leaves litter in
/// the destination directory. A discard that itself fails is surfaced as a
/// rollback failure rather than swallowed: the staged hidden file is left
/// in the destination and the outer rollback can neither retry nor report
/// a partial file it was never told about.
pub(super) fn discard_staged_copy(
    staged: PreparedWriteTarget,
    context: &'static str,
    error: io::Error,
) -> TransferError {
    let staged_path = staged.staged_path().to_path_buf();
    if let Err(discard_error) = staged.rollback() {
        return TransferError::RollbackFailed {
            path: staged_path,
            context: format!(
                "{context}: {error}; additionally the staged copy could not be discarded and \
                 was left behind: {discard_error}"
            ),
        };
    }
    transfer_io(context, error)
}
