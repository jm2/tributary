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

/// Detail of what `commit` actually published, for callers that need to log,
/// report, or roll back the publish outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    /// The relative path beneath the retained mount that now names the data.
    pub relative_path: PathBuf,
    /// How the conflict policy was resolved against the live filesystem.
    pub resolution: ConflictResolution,
    /// Relative path of the backup sibling bound to a replaced occupant's
    /// bytes during an Overwrite commit. The bind happens at commit time
    /// against whatever the destination name resolves to in that instant,
    /// so a concurrent writer's file is backed up — never silently replaced
    /// and destroyed. `None` when nothing pre-existing was replaced (a
    /// fresh publish or a preserved sibling).
    pub replaced_original: Option<PathBuf>,
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
    /// The publish DID happen — the staged bytes were renamed to the
    /// destination — but the post-publish mount revalidation failed. The
    /// outcome is carried so the caller can record the publication for
    /// rollback before surfacing the failure: dropping it would leave
    /// committed bytes unrecorded, so a failed transfer could never undo
    /// them.
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
