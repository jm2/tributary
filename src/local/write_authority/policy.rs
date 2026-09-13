//! Conflict and outcome types for the mounted write authority.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::local::root_authority::LeafIdentity;

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

/// What a commit left at the destination, and therefore which reversal the
/// caller's rollback owes.
///
/// This is deliberately a distinct disposition rather than an encoding on
/// [`CommitOutcome::published_leaf`]: an identity-less `published_leaf`
/// already has a legacy meaning (a reversal degrades to path-only
/// behavior), and redefining it would silently change every uncoupled
/// reversal. Only the Windows rebind-exhaustion disposition is an explicit
/// "nothing of the transfer's landed" state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitDisposition {
    /// The staged bytes landed at the destination: a fresh write, a
    /// preserved sibling, or an identity-coupled replacement. Rollback
    /// removes the published leaf or restores an identity-verified backup.
    Published,
    /// Nothing of the transfer's landed at the destination. A Windows
    /// replace publish exhausted its rebind bound against a concurrent
    /// occupant and retained the pre-transfer occupant's backup; the
    /// destination still holds an occupant the transfer never owned (or is
    /// vacant). Rollback may restore the retained backup ONLY into an
    /// absent slot and must refuse any occupant intact.
    DisplacedOnly,
}

/// Detail of what `commit` actually published, for callers that need to log,
/// report, or roll back the publish outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    /// The relative path beneath the retained mount that now names the data.
    pub relative_path: PathBuf,
    /// How the conflict policy was resolved against the live filesystem.
    pub resolution: ConflictResolution,
    /// What the commit left at the destination, and therefore which
    /// reversal rollback owes. `Published` for every ordinary commit;
    /// `DisplacedOnly` only for the Windows rebind-exhaustion disposition,
    /// where `replaced_original` names a retained displaced occupant but no
    /// transfer bytes were published.
    pub disposition: CommitDisposition,
    /// Relative path of the backup sibling bound to a replaced occupant's
    /// bytes during an Overwrite commit. The bind happens at commit time
    /// against whatever the destination name resolves to in that instant,
    /// so a concurrent writer's file is backed up — never silently replaced
    /// and destroyed. `None` when nothing pre-existing was replaced (a
    /// fresh publish or a preserved sibling).
    pub replaced_original: Option<PathBuf>,
    /// No-follow identity of the replaced occupant, captured from the
    /// object itself at the moment the backup was bound to it — the same
    /// instant `replaced_original` was created. Rollback restoration and
    /// successful-transfer cleanup verify the backup still names THIS
    /// object before moving or discarding it: a backup sibling that a
    /// concurrent writer swapped for a foreign object is refused
    /// fail-closed (never installed as the original, never deleted). The
    /// comparison is object-coupled (device and index, or volume and file
    /// id) rather than exact, because the bind and the atomic swap
    /// legitimately update the bound object's change-sensitive instant.
    /// `None` only when the platform could not capture an identity; such
    /// a backup degrades to the legacy path-only handling.
    pub replaced_original_leaf: Option<LeafIdentity>,
    /// No-follow identity of the published leaf, bound to the staged object
    /// immediately before the winning publish (the rename preserves the
    /// object, so this names exactly what the transfer published — not
    /// whatever a concurrent writer may have put at the destination name
    /// afterwards). Rollback reversals
    /// compare this against whatever occupies the path before removing or
    /// restoring over it, so a concurrent writer's replacement is detected
    /// and refused instead of destroyed by pathname alone. `None` when the
    /// identity could not be captured (the platform has no identity
    /// primitive, or the staged leaf could not be read in the instant
    /// before the publish); a
    /// reversal of an identity-less record degrades to the legacy
    /// path-only behavior.
    pub published_leaf: Option<LeafIdentity>,
}

/// Failure of a staged-write commit.
#[derive(Debug, Error)]
pub enum CommitError {
    /// The publish itself never happened: the staged file was not renamed
    /// to its destination and nothing was changed there.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The publish reached a state the caller MUST record before the
    /// failure surfaces. Either the staged bytes WERE renamed to the
    /// destination but a post-publish verification failed — dropping the
    /// outcome would leave committed bytes unrecorded, so a failed
    /// transfer could never undo them — or a Windows replace publish
    /// exhausted its rebind bound with a completed binding retained:
    /// nothing landed, but the displaced original survives at its verified
    /// backup and the outcome carries it (with
    /// [`CommitDisposition::DisplacedOnly`]) so the caller records the
    /// replacement for rollback or disposal instead of stranding a hidden
    /// orphan. A `DisplacedOnly` outcome must never be reversed with the
    /// identity-coupled replacement path: no transfer bytes were ever
    /// published, so its restoration is valid only into an absent slot.
    #[error("staged file was published but post-publish verification failed: {error}")]
    PublishVerification {
        /// What was published, including the backup bind of a replaced
        /// occupant and the published leaf's identity.
        outcome: CommitOutcome,
        /// The verification failure.
        #[source]
        error: io::Error,
    },
}
