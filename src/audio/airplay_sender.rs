//! AirPlay sender seam.
//!
//! `airplay_output` used to build every session directly on a GStreamer
//! `raopsink` pipeline. That shape made a whole class of maintained sender
//! candidates unrepresentable: a process adapter (a daemon fed over a pipe)
//! is not a `gst::Element`, and the previous `build_sink_tail(...) ->
//! gst::Element` seam could never express one.
//!
//! This module is the GStreamer-independent contract the design investigation
//! ([`docs/airplay-sender-design.md`], §4.1) specifies:
//!
//! - [`AirplaySender`] is one immutable transmission path per backend,
//!   chosen at load time. [`AirplaySender::probe`] is the fail-closed
//!   availability gate that runs before any per-track media work, and
//!   [`AirplaySender::open_session`] negotiates one live session.
//! - [`SenderSession`] owns the transport for one track. It pushes PCM and
//!   control and it publishes a non-blocking [`SenderPosition`] snapshot.
//! - [`SenderWriteOutcome`] separates retryable backpressure from terminal
//!   session loss so a decode pump never spins on a dead session.
//! - [`SenderError`] is the stable, machine-distinguishable failure taxonomy
//!   the load path branches on; each variant still carries the
//!   user-actionable, localized message the load path surfaces verbatim.
//!
//! # Scope of this record
//!
//! This module lands the seam with the existing GStreamer `raopsink` path as
//! the first [`AirplaySender`]. Behavior is unchanged for every existing
//! load: the availability gate still runs before media preparation, and a
//! load still publishes the same generation-tagged events.
//!
//! The recovery-custody capability the design attaches to the seam
//! (`MediaTicketCustody`, `InFlightCancelRegistration`, and the
//! `SenderError::RecoveryPending` outcome) is deliberately deferred to the
//! process-adapter record: it exists to unwind a *transmitted mutating
//! daemon RPC*, and the GStreamer adapter never transmits one. The
//! [`OpenCancel`] currency is landed now because the load path owns it on
//! every backend, and it is the signal an in-flight open observes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(test)]
use std::time::{Duration, Instant};

use super::{PlayerEventGeneration, PlayerState};

/// Outcome of one [`SenderSession::write_pcm`] call.
///
/// A byte count alone cannot express both retryable backpressure and terminal
/// session loss, and a daemon-adapter FIFO produces hard write errors (a
/// closed reader fails with `EPIPE`) that must never be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SenderWriteOutcome {
    /// `n` bytes accepted; the caller continues with the remainder.
    Accepted(usize),
    /// Healthy but momentarily full; wake and retry.
    Backpressure,
    /// The session failed terminally. The adapter has already published the
    /// generation-tagged error + `Stopped` events before returning this.
    Terminal(String),
}

/// The latest position/duration snapshot a session has published.
///
/// Values are meaningful only for the generation the session was opened for;
/// the publisher drops snapshots from any other generation. `position_ms` is
/// `None` while the adapter has no trustworthy value. `stale` is set once the
/// underlying source can no longer be confirmed, so a snapshot freezes at its
/// last confirmed values and is never extrapolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SenderPosition {
    pub(super) generation: PlayerEventGeneration,
    pub(super) position_ms: Option<u64>,
    pub(super) duration_ms: Option<u64>,
    pub(super) stale: bool,
}

impl SenderPosition {
    /// A snapshot with no confirmed sample for `generation`.
    pub(super) fn unknown(generation: PlayerEventGeneration) -> Self {
        Self {
            generation,
            position_ms: None,
            duration_ms: None,
            stale: false,
        }
    }
}

/// Terminal outcome of the serialized recovery behind a
/// `SenderError::RecoveryPending` open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecoveryOutcome {
    /// Restoration ran and the incomplete-takeover record cleared.
    Restored,
    /// Restoration did not complete; the record was retained for the
    /// supervisor. Terminal, and explicitly not a success.
    RestorationFailed { message: String },
    /// Quiescence was never established (or no recovery owner could be
    /// started), so no terminal restoration ran and an outstanding mutation
    /// may still land. The advisory lock and the route custody are retained
    /// for the supervisor; the caller must **not** release its ticket.
    /// Terminal, and explicitly not a success.
    Retained { message: String },
}

/// Completion handle for a serialized recovery that a
/// `SenderError::RecoveryPending` open leaves behind.
///
/// Cloneable and safe to hold across threads. [`Self::wait`] always ends in a
/// terminal [`RecoveryOutcome`]; it never blocks indefinitely, because the
/// recovery it describes is bounded by the adapter's settle-or-restart
/// quiescence and its recovery deadline.
#[derive(Debug, Clone, Default)]
pub(super) struct RecoveryCompletion {
    state: Arc<(Mutex<Option<RecoveryOutcome>>, Condvar)>,
}

impl RecoveryCompletion {
    /// A handle already resolved to `outcome`.
    pub(super) fn resolved(outcome: RecoveryOutcome) -> Self {
        Self {
            state: Arc::new((Mutex::new(Some(outcome)), Condvar::new())),
        }
    }

    /// Record the terminal outcome and wake every waiter. Idempotent: the
    /// first terminal outcome wins.
    pub(super) fn resolve(&self, outcome: RecoveryOutcome) {
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            *guard = Some(outcome);
            cvar.notify_all();
        }
    }

    /// Block until the serialized recovery reaches its terminal state.
    /// Returns immediately once resolved.
    pub(super) fn wait(&self) -> RecoveryOutcome {
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(outcome) = guard.as_ref() {
                return outcome.clone();
            }
            guard = cvar
                .wait(guard)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

/// Stable, machine-distinguishable seam failures.
///
/// The load path's deadline, dependency, authentication, and receiver-failure
/// contracts branch on the variant; each still carries the user-actionable,
/// localized message the load path surfaces verbatim.
#[derive(Debug, Clone)]
pub(super) enum SenderError {
    /// A documented probe/open deadline was exceeded and the server side
    /// quiesced inside the cleanup deadline, so the attempt was torn down
    /// before this variant was returned.
    Deadline(String),
    /// The cleanup deadline was missed while a transmitted mutating daemon
    /// RPC was still unsettled. Deliberately not a clean unwind: the
    /// incomplete-takeover record stays in place and recovery stays
    /// serialized until the request settles. Carries the completion handle
    /// the load path awaits before releasing its own ticket.
    RecoveryPending {
        message: String,
        completion: RecoveryCompletion,
    },
    /// The dependency is absent, unreachable, unsupported on this platform,
    /// or not the dedicated Tributary-owned instance the adapter requires.
    Dependency(String),
    /// The receiver requires pairing, a password, or PIN verification that
    /// has not succeeded.
    Authentication(String),
    /// The receiver-side session failed after negotiation began.
    Receiver(String),
}

impl SenderError {
    /// The localized, user-actionable message to surface verbatim.
    pub(super) fn message(&self) -> &str {
        match self {
            Self::Deadline(message)
            | Self::Dependency(message)
            | Self::Authentication(message)
            | Self::Receiver(message) => message,
            Self::RecoveryPending { message, .. } => message,
        }
    }
}

/// Cancellation currency for one `open_session` call, owned by the load path.
///
/// Setting it is synchronous, idempotent, and safe from any thread. After
/// [`Self::cancel`] returns, an in-flight `open_session` observes the flag
/// immediately and unwinds through its restoration path. This is deliberately
/// not `PlayerEventGeneration`: a generation is a `Copy` value the caller
/// compares after the fact, not a flag an in-flight call can observe.
#[derive(Clone, Default)]
pub(super) struct OpenCancel {
    flag: Arc<AtomicBool>,
}

impl OpenCancel {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Set the flag. Idempotent and safe from any thread.
    pub(super) fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Cheap non-blocking check.
    pub(super) fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// A shared Stop/start boundary for one live session (review U3, review V1).
///
/// The load path (which owns the controller) and the session both hold the
/// same gate, but the gate separates two concerns that review U3 had coupled:
///
/// 1. **Authorization** — "may a start effect run, and is it the one a Stop
///    must settle?" This is a single serialized decision under the gate's
///    mutex. A Stop and an authorization cannot interleave, so a start is
///    either authorized *before* a Stop (and then settled by the session's own
///    worker teardown) or refused *after* it. There is no check-to-effect
///    window.
/// 2. **Execution** — the effect itself (a blocking daemon RPC, or a pipeline
///    state change). This runs **outside** the mutex, on the worker that owns
///    it, so a concurrent Stop never blocks behind network work (review V1).
///
/// A Stop only records cancellation and returns; it never waits for an
/// in-flight effect. Settlement of an effect that transmitted before the Stop
/// is owned by the worker that ran it (its session teardown), not by the UI
/// Stop path.
pub(super) struct SessionGate {
    state: Mutex<SessionGateState>,
    /// Woken when an authorized start effect drains, so a worker (or a test)
    /// can observe settlement without polling. The UI Stop path never waits
    /// on this.
    drained: Condvar,
}

#[derive(Default)]
struct SessionGateState {
    stopped: bool,
    /// Authorized start effects that are currently running their effect. A
    /// Stop does not wait for these; the worker that owns them settles them.
    in_flight: usize,
    /// A start effect ran and obtained a playing result. Retained so a
    /// transmitted-but-cancelled start is still knowable.
    playing: bool,
}

impl SessionGate {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(SessionGateState::default()),
            drained: Condvar::new(),
        }
    }

    /// Authorize and perform one start effect. The decision (check that no
    /// Stop has won, and reserve an in-flight slot) is one serialized step
    /// under the mutex; the effect then runs **outside** the mutex so a
    /// concurrent [`Self::stop`] returns promptly. Returns `false` — and does
    /// not run the effect — once a Stop has won the boundary, and also returns
    /// `false` when a Stop wins *after* authorization (the effect may have
    /// transmitted, but it is not reported as a live start; the worker's
    /// teardown settles it).
    pub(super) fn start<F>(&self, effect: F) -> bool
    where
        F: FnOnce() -> bool,
    {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.stopped {
                return false;
            }
            state.in_flight += 1;
        }
        // The effect runs without the state lock: Stop must never block behind
        // it (review V1).
        let started = effect();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.in_flight -= 1;
        if started {
            state.playing = true;
        }
        let stopped = state.stopped;
        self.drained.notify_all();
        // A Stop that landed while the effect was in flight suppresses the
        // accepted result; the effect has already been (or is being)
        // transmitted, and the session's worker teardown compensates it
        // (review V1). Authorization-before-Stop still settles.
        started && !stopped
    }

    /// Record cancellation and return promptly. Any start effect already
    /// authorized may still be transmitting; the worker that owns it settles
    /// it, so this never waits on network work (review V1).
    pub(super) fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.stopped = true;
        // Wake any test/worker bounded wait promptly.
        self.drained.notify_all();
    }

    /// `true` once a Stop has won the boundary.
    pub(super) fn is_stopped(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).stopped
    }

    /// Bounded wait until every authorized start effect has drained. A test
    /// aid: the UI Stop path must never wait on this, and the worker that owns
    /// an effect settles it on its own thread before its teardown completes
    /// (review V1).
    #[cfg(test)]
    pub(super) fn wait_effects_drained(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.in_flight != 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self
                .drained
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
        }
        true
    }
}

/// One live AirPlay session, already negotiated with the receiver.
///
/// Implementations own their transport; Tributary only pushes audio and
/// control. `Send` because sessions outlive the UI thread.
pub(super) trait SenderSession: Send {
    /// Push interleaved s16le 44100 Hz stereo PCM into the session.
    ///
    /// A session that sources its own decoder from the prepared URI owns its
    /// decode internally and returns `Accepted(samples.len())` without
    /// consuming pushed audio.
    fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome;

    /// Receiver-facing volume in `[0.0, 1.0]`; the adapter maps to its
    /// protocol's convention.
    fn set_volume(&mut self, level: f64);

    fn pause(&mut self);

    /// Resume playback. Returns `true` when the session actually started
    /// playing and `false` when it could not (a cancelled acceptance boundary
    /// or a failed start RPC). The worker reports the outcome truthfully
    /// instead of publishing `Playing` unconditionally (review T3).
    fn resume(&mut self) -> bool;

    /// Confirm the start that [`Self::resume`] just accepted and publish its
    /// playback state atomically with the session's own terminal transition
    /// (review X1). Returns the state the worker should cache — `Some` when a
    /// start state may be published, `None` once the session has gone terminal
    /// and nothing may follow the terminal `Stopped`/`TrackEnded`.
    ///
    /// The default returns `Some(PlayerState::Playing)` without publishing, so
    /// the worker caches and publishes it; that is correct for sessions with no
    /// internal terminal transition. A session that owns an internal terminal
    /// transition implements this so the publication is serialized with it.
    fn confirm_started(&self) -> Option<PlayerState> {
        Some(PlayerState::Playing)
    }

    /// Flush buffered audio without tearing down the receiver session.
    fn flush(&mut self);

    /// Nonblocking position/duration observation. Performs no I/O and never
    /// blocks the caller.
    fn observe(&self) -> SenderPosition;

    /// The current playback state for the output abstraction.
    fn state(&self) -> PlayerState;

    /// Tear down the receiver session and local resources. Consumes `self` so
    /// a closed session is unrepresentable.
    fn close(self: Box<Self>);
}

/// Outcome of [`AirplaySender::open_session`].
pub(super) enum OpenOutcome {
    /// Negotiation completed; the session is live.
    Opened(Box<dyn SenderSession>),
    /// Cancellation was observed before negotiation completed and the
    /// operation in flight was aborted rather than awaited. Terminal,
    /// fully-unwound state.
    Cancelled,
    /// A failure inside the seam's taxonomy.
    Failed(SenderError),
}

/// The receiver a load targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SenderTarget {
    pub(super) display_name: String,
    pub(super) host: String,
    pub(super) port: u16,
    /// The normalized device identifier (MAC/`deviceid`) retained from
    /// discovery. `None` when discovery did not publish one. A process
    /// adapter that maps by identifier fails closed rather than name-match.
    pub(super) device_id: Option<String>,
}

impl SenderTarget {
    pub(super) fn new(
        display_name: &str,
        host: &str,
        port: u16,
        device_id: Option<String>,
    ) -> Self {
        Self {
            display_name: display_name.to_string(),
            host: host.to_string(),
            port,
            device_id,
        }
    }
}

/// Per-track context for one [`AirplaySender::open_session`] call.
///
/// A sender is an immutable instance per backend and owns no per-track state,
/// so everything a session needs to wire its event forwarding and its
/// app-owned media ticket is threaded through here. `prepared_uri` is the
/// credential-safe loopback URL the proxy minted; the original authenticated
/// URL never reaches the sender.
///
/// The context is **owned** (no borrows) so the load path can move it onto the
/// per-load worker that performs the blocking negotiation off the UI thread
/// without blocking GTK (review F2). `open_id` is the load's stable identity
/// for keyed in-flight cancellation registration.
pub(super) struct SenderOpenContext {
    pub(super) target: SenderTarget,
    pub(super) prepared_uri: String,
    pub(super) event_tx: async_channel::Sender<super::PlayerEvent>,
    pub(super) generation: PlayerEventGeneration,
    pub(super) media_proxy: Arc<super::gstreamer_media::GstreamerMediaProxy>,
    /// The app-owned loopback media ticket for this load, when the media was
    /// protected. The opened session adopts it so EOS/error/close revoke it
    /// by identity; a non-opened outcome leaves it to the load path to
    /// release.
    pub(super) media_ticket: Option<Arc<super::gstreamer_media::GstreamerMediaTicket>>,
    pub(super) volume: f64,
    pub(super) cancel: OpenCancel,
    /// The load path's Stop/start boundary for this session (review U3). A
    /// session authorizes its playback start through this gate so a Stop taken
    /// by the load path serializes with the start effect instead of racing it.
    pub(super) session_gate: Arc<SessionGate>,
    /// Stable per-load identity used to key in-flight cancellation
    /// registration in the media proxy.
    pub(super) open_id: u64,
}

/// A selectable transmission path. One immutable instance per
/// protocol/backend, chosen at load time by configuration.
pub(super) trait AirplaySender: Send + Sync {
    /// Stable identifier for logs and diagnostics.
    fn name(&self) -> &'static str;

    /// `Ok(())` when this sender can transmit on this host. `Err` is a
    /// [`SenderError`]: the variant tells callers which failure kind fired,
    /// and its payload is the user-actionable guidance the load path surfaces
    /// verbatim. Bounded by contract: it performs only documented
    /// discovery/health checks, enforces a documented deadline, and never
    /// blocks unboundedly.
    fn probe(&self) -> Result<(), SenderError>;

    /// Negotiate a session for `ctx`. Called only after `probe` succeeded and
    /// media was prepared.
    ///
    /// `ctx.cancel` is the load's cancellation currency; an implementation
    /// that performs blocking negotiation must observe it (racing each
    /// blocking step against it) and return [`OpenOutcome::Cancelled`] after
    /// its restoration path completes.
    fn open_session(&self, ctx: &SenderOpenContext) -> OpenOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_cancel_is_idempotent_and_visible() {
        let cancel = OpenCancel::new();
        assert!(!cancel.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn open_cancel_clones_share_one_flag() {
        let cancel = OpenCancel::new();
        let clone = cancel.clone();
        clone.cancel();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn recovery_completion_returns_immediately_when_resolved() {
        let completion = RecoveryCompletion::resolved(RecoveryOutcome::Restored);
        assert_eq!(completion.wait(), RecoveryOutcome::Restored);
    }

    #[test]
    fn recovery_completion_wakes_a_waiter_with_the_terminal_outcome() {
        let completion = RecoveryCompletion::default();
        let waiter = completion.clone();
        let handle = std::thread::spawn(move || waiter.wait());
        completion.resolve(RecoveryOutcome::RestorationFailed {
            message: "retained".to_string(),
        });
        assert_eq!(
            handle.join().expect("waiter"),
            RecoveryOutcome::RestorationFailed {
                message: "retained".to_string()
            }
        );
    }

    #[test]
    fn recovery_completion_first_terminal_outcome_wins() {
        let completion = RecoveryCompletion::resolved(RecoveryOutcome::Restored);
        completion.resolve(RecoveryOutcome::RestorationFailed {
            message: "later".to_string(),
        });
        assert_eq!(completion.wait(), RecoveryOutcome::Restored);
    }

    #[test]
    fn sender_error_message_is_the_surfaced_guidance() {
        let error = SenderError::Dependency("raopsink missing".to_string());
        assert_eq!(error.message(), "raopsink missing");
        let pending = SenderError::RecoveryPending {
            message: "recovering".to_string(),
            completion: RecoveryCompletion::default(),
        };
        assert_eq!(pending.message(), "recovering");
    }

    #[test]
    fn sender_position_unknown_has_no_sample() {
        let generation = PlayerEventGeneration::from_raw(3);
        let position = SenderPosition::unknown(generation);
        assert_eq!(position.generation, generation);
        assert_eq!(position.position_ms, None);
        assert_eq!(position.duration_ms, None);
        assert!(!position.stale);
    }

    #[test]
    fn sender_target_retains_the_discovery_identifier() {
        let target = SenderTarget::new("Living Room", "192.168.1.2", 7000, Some("AABBCC".into()));
        assert_eq!(target.device_id.as_deref(), Some("AABBCC"));
        assert_eq!(target.host, "192.168.1.2");
        assert_eq!(target.port, 7000);
    }

    /// U3: the shared Stop/start boundary is one serialized decision. A start
    /// runs its effect only while no Stop has won; a Stop that wins first
    /// refuses the effect outright.
    #[test]
    fn session_gate_serializes_stop_and_start() {
        let gate = SessionGate::new();

        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        assert!(gate.start(move || {
            ran_clone.store(true, Ordering::SeqCst);
            true
        }));
        assert!(ran.load(Ordering::SeqCst));

        gate.stop();
        assert!(gate.is_stopped());
        let ran_after_stop = Arc::new(AtomicBool::new(false));
        let ran_after_stop_clone = Arc::clone(&ran_after_stop);
        assert!(!gate.start(move || {
            ran_after_stop_clone.store(true, Ordering::SeqCst);
            true
        }));
        assert!(
            !ran_after_stop.load(Ordering::SeqCst),
            "the start effect must not run after a Stop"
        );
    }

    /// V1: a Stop must not block behind an in-flight start effect. The effect
    /// is parked while a Stop is taken on another thread; the Stop returns
    /// promptly and the effect's result is suppressed (the worker settles it).
    #[test]
    fn session_gate_stop_does_not_block_behind_an_in_flight_start() {
        let gate = Arc::new(SessionGate::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let start_gate = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            start_gate.start(move || {
                let _ = entered_tx.send(());
                // Block "in the effect" until the test releases it.
                let _ = release_rx.recv();
                true
            })
        });

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the start effect must begin");

        // The Stop must return promptly even though the effect is still parked.
        let stop_gate = Arc::clone(&gate);
        let (stop_done_tx, stop_done_rx) = std::sync::mpsc::channel::<()>();
        let stop_thread = std::thread::spawn(move || {
            stop_gate.stop();
            let _ = stop_done_tx.send(());
        });
        stop_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Stop must not block behind an in-flight start effect");
        stop_thread.join().expect("stop thread");

        // Release the effect; the start is suppressed because a Stop won while
        // it was in flight, and the effect is now drained.
        release_tx.send(()).expect("release the effect");
        assert!(
            !handle.join().expect("start thread"),
            "a Stop that lands during the effect suppresses the accepted start"
        );
        assert!(gate.wait_effects_drained(Duration::from_secs(5)));
    }

    /// V1: after a Stop, no new start is authorized, and a start authorized
    /// before the Stop still reports a drained (settled) effect boundary.
    #[test]
    fn session_gate_records_an_authorized_effect_until_settled() {
        let gate = SessionGate::new();
        assert!(gate.start(|| true));
        assert!(gate.wait_effects_drained(Duration::from_secs(1)));
        gate.stop();
        assert!(!gate.start(|| true));
        assert!(gate.is_stopped());
    }
}
