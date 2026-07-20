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

use std::cell::RefCell;
use std::rc::Rc;

use crate::local::engine::LibraryCommand;

struct AdmissionInner {
    open: bool,
    tx: async_channel::Sender<LibraryCommand>,
}

/// Cloneable, GTK-main-thread admission boundary for library commands.
#[derive(Clone)]
pub(super) struct LibraryCommandAdmission {
    inner: Rc<RefCell<AdmissionInner>>,
}

impl LibraryCommandAdmission {
    /// Create an unbounded FIFO and its sole UI-side admission boundary.
    pub(super) fn channel() -> (Self, async_channel::Receiver<LibraryCommand>) {
        let (tx, rx) = async_channel::unbounded();
        (
            Self {
                inner: Rc::new(RefCell::new(AdmissionInner { open: true, tx })),
            },
            rx,
        )
    }

    /// Whether normal command admission remains open.
    pub(super) fn is_open(&self) -> bool {
        self.inner.borrow().open
    }

    /// Queue one ordinary mutation only while admission remains open.
    pub(super) fn try_send(&self, command: LibraryCommand) -> bool {
        let inner = self.inner.borrow();
        inner.open && inner.tx.try_send(command).is_ok()
    }

    /// Atomically close ordinary admission and append the terminal FIFO marker.
    ///
    /// All clones share the same `RefCell`, and every caller runs on GTK's main
    /// thread. No callback can interleave between closing the gate and queuing
    /// `Flush`, while every later producer observes `open == false`.
    pub(super) fn close_and_flush(&self, completion: async_channel::Sender<()>) -> bool {
        let mut inner = self.inner.borrow_mut();
        if !inner.open {
            return false;
        }
        inner.open = false;
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
    fn close_rejects_post_marker_work_while_flush_is_pending() {
        let (admission, rx) = LibraryCommandAdmission::channel();
        assert!(admission.try_send(rating_command("rating-before-close", Some(72))));
        assert!(admission.try_send(history_command("history-before-close", 1)));

        let (completion_tx, completion_rx) = async_channel::bounded(1);
        assert!(admission.close_and_flush(completion_tx));
        assert!(!admission.is_open());
        assert!(!admission.try_send(rating_command("rating-after-close", None)));
        assert!(!admission.try_send(history_command("history-after-close", 2)));
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
        assert!(!admission.try_send(rating_command("rating-still-closed", Some(50))));
        assert!(!admission.try_send(history_command("history-still-closed", 3)));
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
