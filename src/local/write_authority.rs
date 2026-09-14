//! Retained write authority for one exact mounted filesystem.
//!
//! This module adds write capability on top of the read-only
//! [`MountedRootAuthority`]. The same retained root handle, parent chain,
//! mount generation, and filesystem boundary policy gate every byte written
//! beneath the mount. There is no shared-marker requirement; mounted authority
//! exists for ephemeral portable devices and replaces its session epoch on
//! relocation, pre-unmount, or removal.
//!
//! The intended consumer is the generic mounted-filesystem transfer planner
//! in [`crate::device::transfer`]. It must never be used to author a library
//! root (those keep their marker-backed
//! [`RootAuthorityLease`](super::root_authority::RootAuthorityLease) and
//! explicit database enrollment).
//!
//! File writes are staged: a sibling temporary file is created with
//! `O_CREAT | O_EXCL | O_NOFOLLOW` (Unix) or with the reparse-point attribute
//! rejected (Windows), then renamed atomically once
//! [`PreparedWriteTarget::commit`] is called. The staged handle is closed
//! before the rename or removal — Windows refuses to rename or delete a file
//! while a handle without `FILE_SHARE_DELETE` is open, so the handle never
//! outlives the write phase. A rollback drops the staged file. The
//! destination filesystem is observed through the same retained boundary, so
//! a binder swap or remount between staging and commit is detected and
//! refused without surfacing a partial publish.

mod authority;
mod policy;
mod staging;
mod target;

#[cfg(test)]
mod tests;

// The write-authority API is consumed through this module root by the
// transfer planner and the upcoming sync callers; until then the re-export
// carries the intended public surface.
#[allow(unused_imports)]
pub use authority::MountedWriteAuthority;
#[allow(unused_imports)]
pub use policy::{CommitOutcome, ConflictPolicy, ConflictResolution};
#[allow(unused_imports)]
pub use target::{MountedDirectory, PreparedWriteTarget};
