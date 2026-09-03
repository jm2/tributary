//! Generic mounted-filesystem transfer planner and executor.
//!
//! Issue #8 / P3.2 requires a generic mounted-filesystem transfer planner and
//! executor with retained write authority, capacity and conflict policy,
//! atomic copy where possible, progress, cancellation, and rollback. The
//! planner and executor here satisfy every one of those requirements without
//! coupling to a specific discovery backend, MTP device, or sync schedule.
//!
//! The intended caller is the future device-sync UX. The mount-relative scan
//! and authority model are the same as those used by the removable-media
//! scanner ([`crate::removable`]) and the resolver
//! ([`crate::local::resolver`]). A successful scan is followed by a transfer
//! plan; an admitted plan is committed through the destination's
//! [`MountedWriteAuthority`](crate::local::write_authority::MountedWriteAuthority)
//! and the source's [`MountedRootAuthority`](crate::local::root_authority::MountedRootAuthority).
//!
//! ## Authority
//!
//! The source authority is read-only; every source file is opened through
//! [`MountedRootAuthority::open_relative_regular_file`]. The destination is
//! the write authority. Each write is staged as a sibling temporary file
//! inside the destination directory and committed with a single `rename(2)`.
//! Both authorities are revalidated before and after every operation, so a
//! binder swap, unmount, or remount between staging and commit produces a
//! fail-closed error rather than a partial publish.
//!
//! ## Atomicity
//!
//! A staged file is committed with the platform's atomic rename
//! (`rename(2)` on Unix, `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on
//! Windows). The plan records the [`Stage::atomic`] flag so callers and
//! reviewers can identify non-atomic operations (e.g. cross-filesystem moves
//! that the planner chose to surface as a fallible copy).
//!
//! ## Rollback
//!
//! The executor records every published stage. On cancellation or a failed
//! stage, already-committed files are rolled back in reverse order. Each
//! rollback revalidates the destination authority before deletion so a
//! remount between commit and rollback cannot authorise removal of a
//! replacement file that occupies the old path.
//!
//! ## Cancellation
//!
//! Cancellation is cooperative. The caller supplies a
//! [`CancellationObserver`](crate::source_lifecycle::CancellationObserver) and
//! the executor checks it between stages. A long-running file copy checks at
//! every buffered chunk. Cancellation does not abort an in-flight
//! `commit(2)`; the staged file is the unit of work, and an uncommitted
//! staged file is rolled back before the executor returns.

mod executor;
mod planner;
mod types;

#[cfg(test)]
mod executor_tests;
#[cfg(test)]
mod planner_tests;
#[cfg(test)]
mod test_support;

// The planner/executor API is consumed through this module root by the
// upcoming device-sync callers; until then the re-export carries the
// intended public surface.
#[allow(unused_imports)]
pub use executor::TransferExecutor;
#[allow(unused_imports)]
pub use planner::TransferPlanner;
#[allow(unused_imports)]
pub use types::{
    Stage, TransferError, TransferItem, TransferPlan, TransferProgress, TransferRequest,
    TransferSummary,
};
