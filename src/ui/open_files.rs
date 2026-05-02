//! Pending-file queue for the OS "Open With" / `xdg-open` delivery path.
//!
//! `connect_open` (in [`crate::main`]) and `build_window` (in
//! [`crate::ui::window`]) both run on the GTK main thread, so a thread-local
//! queue is enough.  Paths arriving before the window is fully built sit
//! here until the window's `play-pending-files` GAction drains them; paths
//! arriving while the app is already running are appended and the action
//! is fired immediately.

use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    static PENDING: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// Push file paths into the pending queue.
pub fn enqueue<I: IntoIterator<Item = PathBuf>>(paths: I) {
    PENDING.with(|q| q.borrow_mut().extend(paths));
}

/// Take all currently-queued paths, leaving the queue empty.
pub fn drain() -> Vec<PathBuf> {
    PENDING.with(|q| std::mem::take(&mut *q.borrow_mut()))
}
