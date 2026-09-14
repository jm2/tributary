//! Shared fixtures for the transfer planner and executor regressions.
//!
//! Every test runs against its own randomized temporary root supplied by
//! [`tempfile::tempdir`]; dropping the guard removes the tree, so no explicit
//! cleanup is needed.

use std::path::Path;
use std::sync::Arc;

use crate::local::root_authority::MountedRootAuthority;
use crate::local::write_authority::MountedWriteAuthority;

/// Acquire a read authority rooted at a temporary directory.
pub fn read_authority(root: &Path) -> Arc<MountedRootAuthority> {
    Arc::new(MountedRootAuthority::acquire(root).expect("acquire read authority"))
}

/// Acquire a matched read/write authority pair rooted at one temporary
/// directory so scans and commits observe the same mount generation.
pub fn authority_pair(root: &Path) -> (Arc<MountedRootAuthority>, MountedWriteAuthority) {
    let mounted = MountedRootAuthority::acquire(root).expect("acquire mounted");
    let read = Arc::new(mounted);
    let write = MountedWriteAuthority::from_mounted(Arc::clone(&read));
    (read, write)
}

/// Write a source fixture file below `root`, creating parent directories.
pub fn write_source_file(root: &Path, relative: &str, contents: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent");
    std::fs::write(path, contents).expect("write source");
}
