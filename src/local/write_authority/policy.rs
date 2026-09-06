//! Conflict and outcome types for the mounted write authority.

use std::path::PathBuf;

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
/// report, roll back, or clean up the publish outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    /// The relative path beneath the retained mount that now names the data.
    /// This is the actual published path: for a `Preserved` resolution it is
    /// the disambiguated sibling, not the originally requested destination.
    pub relative_path: PathBuf,
    /// How the conflict policy was resolved against the live filesystem.
    pub resolution: ConflictResolution,
    /// Relative path of the saved original moved aside before an `Overwrite`
    /// publish, when a pre-existing destination was present. The caller owns
    /// it from commit onward: restore it on rollback, delete it once the
    /// whole transfer succeeds. `None` for fresh and preserved writes and for
    /// an overwrite whose destination vanished before commit.
    pub backup_relative_path: Option<PathBuf>,
}
