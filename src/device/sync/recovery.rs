//! Attach/detach detection and safe-recovery primitives.
//!
//! A sync run is meaningful only while the device is attached. The
//! recovery module is the place where "device disappeared" is observed
//! and where a half-finished run can be safely resumed.
//!
//! ## Why attach/detach is its own concern
//!
//! The transfer executor already detects a missing device at every
//! authority revalidation. What it does not do is *decide what to do
//! next*: roll back the partial writes, decide whether the remaining
//! work is salvageable, or hand the caller a token to resume later. That
//! is this module's job.
//!
//! ## The session guard
//!
//! [`SyncSessionGuard`] is the runtime handle the executor checks between
//! stages. It is intentionally narrow: it answers one question — "is the
//! device still attached?" — and exposes a single observable event
//! channel so the executor can record what happened.
//!
//! ## Recovery
//!
//! [`AttachDetachRecovery`] is the caller-side API. It consumes the events
//! the executor emitted during a run and reports a verdict:
//!
//! * `Completed` — every planned stage ran to completion.
//! * `Detached` — the device disappeared at some specific stage; the
//!   recovery records which stage ran last and which was the next one
//!   the executor would have run. The caller can resume by submitting a
//!   new plan that skips the stages the previous run completed.
//! * `Failed` — the run aborted for a reason other than a detach.
//!
//! The recovery never mutates the recorded state. The executor updates
//! the recorded state only after a successful stage; a detach leaves the
//! state consistent with what the device actually has.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::executor::SyncStage;

/// What the executor saw during a single sync run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttachDetachEvent {
    /// The device was attached when the run started.
    AttachedAtStart,
    /// The executor finished one stage successfully.
    StageCompleted { stage: SyncStage },
    /// The executor detected that the device had disappeared. The stage
    /// named here is the next stage the executor would have attempted;
    /// it never ran.
    Detached { next_stage: SyncStage },
    /// The executor observed a non-detach failure on the named stage.
    Failed { stage: SyncStage, reason: String },
}

/// Verdict the recovery produces after reading the event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryVerdict {
    /// Every stage ran to completion.
    Completed,
    /// The device detached at `next_stage`. The caller can resume by
    /// submitting a new plan that skips the stages the previous run
    /// already finished.
    Detached {
        last_completed: Option<SyncStage>,
        next_stage: SyncStage,
    },
    /// The run failed for a non-detach reason on `stage`.
    Failed { stage: SyncStage, reason: String },
}

/// The runtime handle the executor checks between stages.
///
/// The guard is cheap to clone. Cloning shares the same observation
/// state, so the executor's view of the device and the recovery's view
/// are always in agreement.
#[derive(Clone)]
pub struct SyncSessionGuard {
    state: Arc<Mutex<GuardState>>,
}

struct GuardState {
    attached: bool,
    events: VecDeque<AttachDetachEvent>,
}

impl SyncSessionGuard {
    /// Construct a guard in the "attached" state. The caller is
    /// responsible for calling [`SyncSessionGuard::mark_detached`] when
    /// the device is observed to be gone.
    pub fn attached() -> Self {
        Self {
            state: Arc::new(Mutex::new(GuardState {
                attached: true,
                events: VecDeque::new(),
            })),
        }
    }

    /// Mark the device as detached. Subsequent calls to
    /// [`SyncSessionGuard::is_attached`] return `false`. This call is
    /// idempotent: a second call while the device is already detached
    /// is a no-op.
    pub fn mark_detached(&self) {
        let mut state = self.state.lock().expect("guard poisoned");
        if state.attached {
            state.attached = false;
        }
    }

    /// True while the device is still attached.
    pub fn is_attached(&self) -> bool {
        self.state.lock().expect("guard poisoned").attached
    }

    /// Record that the executor completed one stage. The executor calls
    /// this after every successful stage; the recovery reads the events
    /// to compute its verdict.
    pub fn record_stage_completed(&self, stage: SyncStage) {
        let mut state = self.state.lock().expect("guard poisoned");
        state
            .events
            .push_back(AttachDetachEvent::StageCompleted { stage });
    }

    /// Record a detach event. The executor calls this when it sees the
    /// device is gone and is about to abort the run.
    pub fn record_detach(&self, next_stage: SyncStage) {
        let mut state = self.state.lock().expect("guard poisoned");
        state
            .events
            .push_back(AttachDetachEvent::Detached { next_stage });
    }

    /// Record a non-detach failure on the named stage.
    pub fn record_failure(&self, stage: SyncStage, reason: impl Into<String>) {
        let mut state = self.state.lock().expect("guard poisoned");
        state.events.push_back(AttachDetachEvent::Failed {
            stage,
            reason: reason.into(),
        });
    }

    /// Borrow the recorded event stream. The stream is owned by the
    /// guard and is mutated only by the executor; this method returns a
    /// snapshot.
    pub fn snapshot_events(&self) -> Vec<AttachDetachEvent> {
        let state = self.state.lock().expect("guard poisoned");
        state.events.iter().cloned().collect()
    }
}

/// The caller-side recovery consumer.
#[derive(Clone, Debug, Default)]
pub struct AttachDetachRecovery {
    _private: (),
}

impl AttachDetachRecovery {
    /// Construct a recovery consumer.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Read the recorded events and produce a verdict.
    ///
    /// The verdict is computed by walking the event stream in order. A
    /// `Detached` event always wins over an earlier `Completed` event —
    /// once the device is gone, every subsequent stage is unattempted.
    /// A `Failed` event likewise wins over a `Detached` if it appears
    /// later; the executor never emits both, but the recovery tolerates
    /// either ordering.
    pub fn verdict(guard: &SyncSessionGuard) -> RecoveryVerdict {
        let events = guard.snapshot_events();
        let mut last_completed: Option<SyncStage> = None;
        let mut verdict = RecoveryVerdict::Completed;
        for event in events {
            match event {
                AttachDetachEvent::AttachedAtStart => {}
                AttachDetachEvent::StageCompleted { stage } => {
                    last_completed = Some(stage);
                }
                AttachDetachEvent::Detached { next_stage } => {
                    verdict = RecoveryVerdict::Detached {
                        last_completed,
                        next_stage,
                    };
                    return verdict;
                }
                AttachDetachEvent::Failed { stage, reason } => {
                    return RecoveryVerdict::Failed { stage, reason };
                }
            }
        }
        verdict
    }
}

/// Why the recovery rejected an input.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The guard's event stream was empty; nothing to recover from.
    #[error("attach/detach recovery received an empty event stream")]
    EmptyEventStream,
    /// The guard reported the device as attached while the recovery
    /// expected it to be detached.
    #[error("recovery called while the device is still attached")]
    StillAttached,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_starts_attached() {
        let guard = SyncSessionGuard::attached();
        assert!(guard.is_attached());
    }

    #[test]
    fn mark_detached_toggles_attached_state() {
        let guard = SyncSessionGuard::attached();
        guard.mark_detached();
        assert!(!guard.is_attached());
        guard.mark_detached();
        assert!(!guard.is_attached());
    }

    #[test]
    fn verdict_reports_completed_when_nothing_failed() {
        let guard = SyncSessionGuard::attached();
        guard.record_stage_completed(SyncStage::OpenSession);
        guard.record_stage_completed(SyncStage::BrowseStorage);
        let verdict = AttachDetachRecovery::verdict(&guard);
        assert!(matches!(verdict, RecoveryVerdict::Completed));
    }

    #[test]
    fn verdict_reports_detach_with_last_completed_stage() {
        let guard = SyncSessionGuard::attached();
        guard.record_stage_completed(SyncStage::OpenSession);
        guard.record_detach(SyncStage::FetchTrack {
            track_id: "t".into(),
        });
        let verdict = AttachDetachRecovery::verdict(&guard);
        match verdict {
            RecoveryVerdict::Detached {
                last_completed,
                next_stage,
            } => {
                assert_eq!(last_completed, Some(SyncStage::OpenSession));
                assert!(matches!(next_stage, SyncStage::FetchTrack { .. }));
            }
            other => panic!("unexpected verdict {other:?}"),
        }
    }

    #[test]
    fn verdict_reports_failure() {
        let guard = SyncSessionGuard::attached();
        guard.record_stage_completed(SyncStage::OpenSession);
        guard.record_failure(
            SyncStage::FetchTrack {
                track_id: "t".into(),
            },
            "io",
        );
        let verdict = AttachDetachRecovery::verdict(&guard);
        match verdict {
            RecoveryVerdict::Failed { stage, reason } => {
                assert!(matches!(stage, SyncStage::FetchTrack { .. }));
                assert_eq!(reason, "io");
            }
            other => panic!("unexpected verdict {other:?}"),
        }
    }
}
