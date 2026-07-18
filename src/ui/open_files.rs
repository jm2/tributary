//! Pending-file queue for the OS "Open With" / `xdg-open` delivery path.
//!
//! `connect_open` (in [`crate::main`]) and `build_window` (in
//! [`crate::ui::window`]) both run on the GTK main thread, so a thread-local
//! queue is enough.  Paths arriving before the window is fully built sit
//! here until the window's `play-pending-files` GAction drains them; paths
//! arriving while the app is already running mint a newer delivery owner and
//! the action is fired immediately. Candidate order within that exact
//! delivery is preserved; an older undrained or in-flight delivery is stale.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic owner of the currently admissible OS-open delivery.
///
/// The queue itself is GTK-thread-local, but admission and tag parsing run on
/// a blocking Tokio worker.  Keeping the generation atomic lets that worker
/// cheaply abandon a delivery after a newer delivery, Stop, output change, or
/// shutdown has superseded it.
static ADMISSION_GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static PENDING: RefCell<Vec<(u64, PathBuf)>> = const { RefCell::new(Vec::new()) };
}

/// One ordered batch drained from the GTK-owned pending queue.
///
/// Deliberately does not implement `Debug`: its paths are OS-delivered
/// capabilities, not log-safe diagnostics.
pub(super) struct PendingDelivery {
    generation: u64,
    paths: Vec<PathBuf>,
}

impl PendingDelivery {
    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn into_paths(self) -> Vec<PathBuf> {
        self.paths
    }

    pub(super) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// A successfully admitted source that has not yet been installed as the GTK
/// playback owner.
///
/// Drop is the failure/closure/panic safety net: until `commit` is called, any
/// stale completion or closed receiver explicitly retires the hidden source.
pub(super) struct PendingExternalSession {
    source_registry: crate::source_registry::SourceRegistry,
    session: Option<crate::source_registry::ExternalFileSession>,
}

impl PendingExternalSession {
    pub(super) fn session(&self) -> &crate::source_registry::ExternalFileSession {
        self.session
            .as_ref()
            .expect("pending external session is committed only once")
    }

    /// Transfer terminal ownership to the playback queue.
    pub(super) fn commit(mut self) {
        let _ = self.session.take();
    }
}

impl Drop for PendingExternalSession {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.source_registry.retire_external(session.source_id());
        }
    }
}

fn next_generation() -> u64 {
    ADMISSION_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

/// Push file paths into the pending queue.
pub fn enqueue<I: IntoIterator<Item = PathBuf>>(paths: I) {
    // Mint ownership before publishing the paths. `enqueue` and `drain` both
    // run on GTK, while a prior blocking worker can observe this invalidation
    // immediately through `is_current`.
    let generation = next_generation();
    PENDING.with(|q| {
        q.borrow_mut()
            .extend(paths.into_iter().map(|path| (generation, path)));
    });
}

/// Take all currently-queued paths, leaving the queue empty.
pub(super) fn drain() -> PendingDelivery {
    let generation = ADMISSION_GENERATION.load(Ordering::Acquire);
    let paths = PENDING.with(|q| {
        std::mem::take(&mut *q.borrow_mut())
            .into_iter()
            .filter_map(|(owner, path)| (owner == generation).then_some(path))
            .collect()
    });
    PendingDelivery { generation, paths }
}

/// Invalidate any in-flight admission without disturbing paths delivered by a
/// future OS-open request. Repeated terminal hooks are intentionally harmless.
pub(super) fn invalidate_admission() {
    next_generation();
}

/// Whether one exact drained delivery still owns admission.
pub(super) fn is_current(generation: u64) -> bool {
    ADMISSION_GENERATION.load(Ordering::Acquire) == generation
}

/// Open and admit the first playable candidate from one exact delivery.
///
/// This function performs blocking filesystem and tag-parser work and must be
/// called only from a blocking worker. Candidates are attempted strictly in OS
/// delivery order. No failure includes or logs a pathname.
pub(super) fn admit_first_playable(
    delivery: PendingDelivery,
    source_registry: crate::source_registry::SourceRegistry,
) -> Option<PendingExternalSession> {
    let generation = delivery.generation();
    for path in delivery.into_paths() {
        if !is_current(generation) {
            return None;
        }

        let Some(display_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let extension = path.extension().and_then(|extension| extension.to_str());
        let Ok(hint) = crate::external_file::ExternalFileHint::new(display_name, extension) else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        if !is_current(generation) {
            return None;
        }
        let Ok(session) =
            source_registry.adopt_external_file_if_current(file, hint, || is_current(generation))
        else {
            continue;
        };
        let pending = PendingExternalSession {
            source_registry,
            session: Some(session),
        };
        if !is_current(generation) {
            return None;
        }
        return Some(pending);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_delivery_preserves_os_order_and_exact_generation() {
        let _ = drain();
        enqueue([PathBuf::from("first"), PathBuf::from("second")]);
        let delivery = drain();
        let generation = delivery.generation();
        assert_eq!(
            delivery.into_paths(),
            [PathBuf::from("first"), PathBuf::from("second")]
        );
        assert!(is_current(generation));

        invalidate_admission();
        assert!(!is_current(generation));

        enqueue([PathBuf::from("stopped")]);
        invalidate_admission();
        assert!(drain().is_empty(), "Stop must not restamp pending paths");
    }
}
