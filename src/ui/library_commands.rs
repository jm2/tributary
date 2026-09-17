//! GTK-thread admission gate for serialized history, rating, root-trust, and
//! Rhythmbox-migration commands.
//!
//! Every producer of the admitted [`LibraryCommand`] variants shares this
//! boundary. Normal shutdown closes admission and appends one terminal FIFO
//! marker in the same synchronous operation, so no admitted mutation can be
//! queued behind that marker. In particular, a Rhythmbox migration admitted
//! before shutdown drains before `Flush`, while a preview or confirmation that
//! finishes after admission closes is rejected. Playlist CRUD and
//! filesystem-watcher mutations use separate boundaries and are not covered
//! here.
//!
//! The FIFO is **bounded**. R9 showed that an initial scan holding read-only
//! discovery could stall command service indefinitely, so ordinary admission
//! is a finite budget: one slot is permanently reserved for the shutdown
//! `Flush` marker and the rest bound how many admitted-but-unserviced commands
//! may be retained. Producers observe an explicit [`CommandAdmissionOutcome`]
//! instead of silently growing a backlog.

use std::cell::RefCell;
use std::rc::Rc;

use tokio_util::sync::CancellationToken;

use crate::local::engine::LibraryCommand;

/// Maximum admitted-but-unserviced library commands.
///
/// One slot is reserved for the terminal `Flush` marker, so this is also the
/// hard ceiling on ordinary commands the engine may be behind by. It is far
/// above any interactive burst (a rating gesture or a playback-history event)
/// yet finite, which is what lets admission report overload instead of
/// accumulating without bound while the scan holds read-only discovery.
pub(super) const COMMAND_FIFO_CAPACITY: usize = 1024;

/// Result of one producer's attempt to admit a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandAdmissionOutcome {
    /// The command is in the FIFO and will be serviced.
    Accepted,
    /// Shutdown closed admission; the command was not retained.
    Closed,
    /// The bounded FIFO is full; the command was not retained. The producer
    /// must surface this to the user (or retry) rather than assume delivery.
    Overloaded,
}

impl CommandAdmissionOutcome {
    pub(super) fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }
}

struct AdmissionInner {
    open: bool,
    tx: async_channel::Sender<LibraryCommand>,
    /// Cooperative cancellation for the engine's initial scan.
    ///
    /// Closing the window must not wait on a scan that is blocked in a
    /// read-only traversal or parser kernel call. `close_and_flush` therefore
    /// cancels this token at the same moment it closes admission, so the scan
    /// stops admitting new durable mutations at its next boundary. The token is
    /// deliberately separate from the command FIFO: the FIFO's `Flush` marker
    /// remains the reserved drain path that the engine must service after the
    /// scan yields.
    scan_cancellation: CancellationToken,
}

/// Cloneable, GTK-main-thread admission boundary for library commands.
#[derive(Clone)]
pub(super) struct LibraryCommandAdmission {
    inner: Rc<RefCell<AdmissionInner>>,
}

impl LibraryCommandAdmission {
    /// Create a bounded FIFO and its sole UI-side admission boundary.
    ///
    /// One slot is reserved for the shutdown `Flush` marker so a saturated
    /// ordinary backlog can never make graceful close impossible.
    pub(super) fn channel() -> (Self, async_channel::Receiver<LibraryCommand>) {
        let (tx, rx) = async_channel::bounded(COMMAND_FIFO_CAPACITY);
        (
            Self {
                inner: Rc::new(RefCell::new(AdmissionInner {
                    open: true,
                    tx,
                    scan_cancellation: CancellationToken::new(),
                })),
            },
            rx,
        )
    }

    /// Whether normal command admission remains open.
    pub(super) fn is_open(&self) -> bool {
        self.inner.borrow().open
    }

    /// Borrow a clone of the scan-cancellation handle for the engine.
    ///
    /// The engine moves this into its runtime task; the UI keeps the sole
    /// admission clone here so window close can cancel the scan without ever
    /// sharing the non-`Send` `RefCell` boundary across threads.
    pub(super) fn scan_cancellation(&self) -> CancellationToken {
        self.inner.borrow().scan_cancellation.clone()
    }

    /// Queue one ordinary mutation if admission is open and has capacity.
    ///
    /// The reserved shutdown slot is never consumed here, so `close_and_flush`
    /// always has room for its terminal marker.
    pub(super) fn try_send(&self, command: LibraryCommand) -> CommandAdmissionOutcome {
        let inner = self.inner.borrow();
        if !inner.open {
            return CommandAdmissionOutcome::Closed;
        }
        let capacity = inner.tx.capacity().unwrap_or(COMMAND_FIFO_CAPACITY);
        if inner.tx.len() >= capacity.saturating_sub(1) {
            return CommandAdmissionOutcome::Overloaded;
        }
        if inner.tx.try_send(command).is_ok() {
            CommandAdmissionOutcome::Accepted
        } else {
            // The only way a send fails after the capacity and open checks is a
            // concurrently closed receiver. Treat it as shutdown.
            CommandAdmissionOutcome::Closed
        }
    }

    /// Atomically close ordinary admission, cancel the initial scan, and append
    /// the terminal FIFO marker.
    ///
    /// All clones share the same `RefCell`, and every caller runs on GTK's main
    /// thread. No callback can interleave between closing the gate and queuing
    /// `Flush`, while every later producer observes `open == false`.
    ///
    /// Cancelling the scan here is what bounds close latency. The scan can only
    /// settle read-only blocking work up to a fixed budget (a `spawn_blocking`
    /// kernel call cannot be interrupted), but it never drops an admitted
    /// durable mutation. Once the scan yields, the engine services the FIFO and
    /// acknowledges `Flush` — the reserved shutdown/drain path.
    pub(super) fn close_and_flush(&self, completion: async_channel::Sender<()>) -> bool {
        let mut inner = self.inner.borrow_mut();
        if !inner.open {
            return false;
        }
        inner.open = false;
        inner.scan_cancellation.cancel();
        // The reserved slot makes this send succeed even when ordinary
        // admission is saturated.
        inner
            .tx
            .try_send(LibraryCommand::Flush { completion })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use crate::architecture::{models::Rating, TrackId};

    use super::*;

    fn history_command(id: &str, counted_at_ms: i64) -> LibraryCommand {
        LibraryCommand::RecordPlaybackHistory {
            track_id: TrackId::new(id).expect("valid history test ID"),
            counted_at_ms,
        }
    }

    fn rating_command(id: &str, rating: Option<u8>) -> LibraryCommand {
        LibraryCommand::SetTrackRating {
            track_id: TrackId::new(id).expect("valid rating test ID"),
            rating: rating.map(|value| Rating::new(value).expect("valid test rating")),
        }
    }

    #[test]
    fn close_cancels_the_initial_scan_while_admission_is_still_open() {
        let (admission, _rx) = LibraryCommandAdmission::channel();

        // A producer that already observed the open gate still gets its
        // command queued; the scan is not cancelled until close.
        let scan_token = admission.scan_cancellation();
        assert!(!scan_token.is_cancelled());
        assert!(admission
            .try_send(rating_command("rating-open", Some(10)))
            .is_accepted());
        assert!(!scan_token.is_cancelled());

        let (completion_tx, _completion_rx) = async_channel::bounded(1);
        assert!(admission.close_and_flush(completion_tx));
        assert!(scan_token.is_cancelled());

        // Every clone shares the token, so the engine's moved clone observes
        // the cancellation raised by the UI thread.
        assert!(admission.scan_cancellation().is_cancelled());
    }

    #[test]
    fn admission_reports_overload_without_consuming_the_shutdown_slot() {
        let (admission, rx) = LibraryCommandAdmission::channel();

        // Fill every ordinary slot; the last slot stays reserved for Flush.
        let ordinary_capacity = COMMAND_FIFO_CAPACITY - 1;
        for index in 0..ordinary_capacity {
            assert_eq!(
                admission.try_send(rating_command(&format!("rating-{index}"), Some(1))),
                CommandAdmissionOutcome::Accepted,
            );
        }

        // One more ordinary command is an explicit overload, not a silent
        // unbounded enqueue.
        assert_eq!(
            admission.try_send(rating_command("rating-overflow", Some(2))),
            CommandAdmissionOutcome::Overloaded,
        );

        // Shutdown still gets its reserved slot and its marker.
        let (completion_tx, completion_rx) = async_channel::bounded(1);
        assert!(admission.close_and_flush(completion_tx));
        assert!(!admission.is_open());
        assert_eq!(
            admission.try_send(rating_command("rating-after-close", None)),
            CommandAdmissionOutcome::Closed,
        );

        // Drain the ordinary commands, then the reserved marker.
        for _ in 0..ordinary_capacity {
            assert!(matches!(
                rx.try_recv(),
                Ok(LibraryCommand::SetTrackRating { .. })
            ));
        }
        let completion = match rx.try_recv() {
            Ok(LibraryCommand::Flush { completion }) => completion,
            other => panic!("expected the reserved FIFO flush, got {other:?}"),
        };
        assert!(matches!(
            rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        completion
            .try_send(())
            .expect("acknowledge the reserved flush");
        completion_rx
            .try_recv()
            .expect("flush acknowledgment reaches shutdown waiter");
    }

    #[test]
    fn close_rejects_post_marker_work_while_flush_is_pending() {
        let (admission, rx) = LibraryCommandAdmission::channel();
        assert!(admission
            .try_send(rating_command("rating-before-close", Some(72)))
            .is_accepted());
        assert!(admission
            .try_send(history_command("history-before-close", 1))
            .is_accepted());

        let (completion_tx, completion_rx) = async_channel::bounded(1);
        assert!(admission.close_and_flush(completion_tx));
        assert!(!admission.is_open());
        assert!(!admission
            .try_send(rating_command("rating-after-close", None))
            .is_accepted());
        assert!(!admission
            .try_send(history_command("history-after-close", 2))
            .is_accepted());
        assert!(matches!(
            completion_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));

        assert!(matches!(
            rx.try_recv(),
            Ok(LibraryCommand::SetTrackRating { track_id, rating })
                if track_id.as_str() == "rating-before-close"
                    && rating == Some(Rating::new(72).unwrap())
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(LibraryCommand::RecordPlaybackHistory { track_id, counted_at_ms: 1 })
                if track_id.as_str() == "history-before-close"
        ));
        let completion = match rx.try_recv() {
            Ok(LibraryCommand::Flush { completion }) => completion,
            other => panic!("expected pending FIFO flush, got {other:?}"),
        };
        assert!(matches!(
            rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(!admission.close_and_flush(async_channel::bounded(1).0));
        assert!(!admission
            .try_send(rating_command("rating-still-closed", Some(50)))
            .is_accepted());
        assert!(!admission
            .try_send(history_command("history-still-closed", 3))
            .is_accepted());
        assert!(matches!(
            rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));

        completion
            .try_send(())
            .expect("acknowledge the still-pending flush");
        completion_rx
            .try_recv()
            .expect("flush acknowledgment reaches shutdown waiter");
    }
}
