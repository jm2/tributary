//! Regressions for the mounted write authority: staged writes, conflict
//! policies, rollback, drop cleanup, and boundary refusal.

mod creation;
mod publish;
mod replace_windows;
mod restore_windows;
mod reversal;
mod staged_lifecycle;

use super::MountedWriteAuthority;

/// Acquire a write authority on a fresh temporary root; dropping the guard
/// removes the tree.
fn authority(root: &tempfile::TempDir) -> MountedWriteAuthority {
    MountedWriteAuthority::acquire(root.path()).expect("acquire write authority")
}

/// Reports whether this process is actually constrained by Unix permission
/// bits. A privileged (euid 0) process bypasses permission checks, so the
/// permission-based failure injection some regressions rely on cannot
/// produce the failure under root; tests using such injection skip when
/// this returns `false` instead of asserting a failure root never observes.
#[cfg(unix)]
fn process_respects_permission_bits() -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Ok(probe) = tempfile::tempdir() else {
        return true;
    };
    if std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o000)).is_err() {
        return true;
    }
    let constrained = std::fs::write(probe.path().join("probe"), b"").is_err();
    let _ = std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o755));
    constrained
}
