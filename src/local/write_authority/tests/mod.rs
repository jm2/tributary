//! Regressions for the mounted write authority: staged writes, conflict
//! policies, rollback, drop cleanup, and boundary refusal.

mod creation;
mod publish;
mod replace_windows;
mod reversal;
mod staged_lifecycle;

use super::MountedWriteAuthority;

/// Acquire a write authority on a fresh temporary root; dropping the guard
/// removes the tree.
fn authority(root: &tempfile::TempDir) -> MountedWriteAuthority {
    MountedWriteAuthority::acquire(root.path()).expect("acquire write authority")
}
