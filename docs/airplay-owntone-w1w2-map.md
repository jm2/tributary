# tr-t3a — OwnTone AirPlay adapter: W1–W2 corrective map

Maps the independent refinery review
(`.gc/operations/reviews/refinery-20260915-91c0a20-tr-t3a/corrective-instructions.md`,
rejected head `91c0a202707a96801d5c6a9999b8ea6169d5ed6e`, base
`754fc6d8e6c7e7b99b1fe8844b73482f833d668d`) to its fix and regression. All
prior dispositions (F1–F5, R1–R5, S1–S7, T1–T6, U1–U6, V1–V3) stand; the
V1–V3 map (`docs/airplay-owntone-v1v3-map.md`) is preserved and this map
supersedes its *Known gaps* section where noted.

The design contract is `docs/airplay-sender-design.md` §4.1/§4.3/§4.4/§9.

| Commit | Scope |
| --- | --- |
| `91c0a202` | V1–V3 (previous leg; rejected W1–W2) |
| *(this leg)* | W1 terminal restoration; W2 production-path terminal/settlement regressions |

## W1 (P1) — terminal restoration must prohibit later control mutations

**Defect.** `restore` serialized restoration against control RPCs via
`mutation_lock`, but restoration was not *terminal*. On EOS or decode error the
pump restored the daemon, removed the takeover record, released the media route
and set `restored = true` without stopping the gate, clearing `running`,
cancelling activation, or terminating the control loop. The worker therefore
kept servicing Pause/Resume/SetVolume, and `transmit_mutation` never inspected
`restored` (or a terminal state) under the lock. A control queued or waiting on
`mutation_lock` while the pump restored could transmit afterwards — `resume`
could re-issue `player/play` against the restored output selection, and the
later `close` restore returned `Ok` immediately on the bare `restored` check,
dropping the instance lock over a newly outstanding (possibly timed-out)
post-restoration mutation.

**Fix (`src/audio/airplay_owntone.rs`).**

- `SessionInner` gains a latched `terminal: AtomicBool`.
- `restore` stores `terminal = true` (and clears `running`) **under
  `mutation_lock`**, *before* any restoring RPC. The terminal transition and
  any concurrent control transmission are therefore one serialized decision.
- `transmit_mutation` refuses — fail-closed, without touching the outstanding
  count — every transmission once `terminal` is set, so a control parked behind
  the terminal restoration acquires the boundary after the transition and sends
  nothing.
- `restore` no longer short-circuits on `restored` alone: if restoration has
  already completed but an outstanding mutation remains, it must first establish
  a confirmed quiescence (`quiesce_daemon`, which drops every in-flight
  request). If that cannot be established it returns `Err` and retains the
  count, so `close` cannot release the instance lock over an unsettled
  mutation.
- Because activation consults `running`/`cancel` and the play effect is a
  `transmit_mutation`, a late `resume` neither transmits `player/play` nor
  publishes `Playing`.

UI Stop remains non-blocking: no wait was added to the Stop path.

**Regressions (HTTP-faithful, real loopback server recording each request
line).**

- `a_control_parked_behind_terminal_restoration_transmits_nothing` — the
  restoring `player/stop` is parked at a real fake daemon; a control is queued
  behind the settlement boundary while the restoration holds it. After the
  restoration completes the control is refused and **no `/api/player/volume`
  request ever reaches the server**.
- `a_terminal_restoration_refuses_a_late_resume_and_sends_no_play` — the
  error-path terminal sequence: after restoration a late `resume` sends **no
  `/api/player/play`** and publishes no `Playing`.
- `a_terminal_restore_does_not_release_over_an_outstanding_mutation` — a second
  (close-path) restore over an unsettled mutation does not return `Ok`; it
  quiesces or fails closed with the count retained.

## W2 (P2) — production-path acceptance regressions

- **Play-stall test observed the real boundary.**
  `a_stop_returns_promptly_while_the_own_tone_play_is_stalled` now reads the
  actual request line at the stalling server, asserts it is
  `PUT /api/player/play` (not a cancelled-before-transmit shortcut), and
  asserts the transmitted-but-timed-out play remains recorded as outstanding,
  so the session's own teardown must settle it. The previous comment-only claim
  that teardown settles receiver state is replaced by an assertion.
- **Retained-recovery spawn-failure/queue preserved.**
  `supervisor_retains_a_retry_owner_until_the_spawn_facility_recovers` now
  additionally asserts, *while every spawn is failing*, that the job remains
  queued and its resources are unreleased — a failed handoff is never reported
  as clean — before the facility recovers and the same production retry path
  releases it.
- **Real `RetainedRecovery` + real advisory lock.**
  `a_retained_recovery_holds_its_real_advisory_lock_until_settlement` takes a
  real `flock` on the instance lock file, hands a `RetainedRecovery` to a
  supervisor whose first owner spawn is injected to fail, then confirms a live
  owner is still proven and that the failed settlement attempt does **not**
  release the advisory lock (a competing opener still cannot take it).

## X1 (P2) — terminal restoration still raced successful control publication

**Review.** `.gc/operations/reviews/refinery-20260915-d83fa9f-tr-t3a/corrective-instructions.md`
at rejected head `d83fa9f94445c72ff428086ccd4405d3374f366e`, base
`754fc6d8e6c7e7b99b1fe8844b73482f833d668d`.

**Defect.** `restore` latched `terminal` under `mutation_lock`, but the
*successful control state publication* happened **after** `transmit_mutation`
released that lock. A concurrent EOS/error terminal restoration could therefore
latch terminal, restore the daemon, and publish `Stopped`/`TrackEnded` *between*
a control's RPC settlement and its `Playing`/`Paused` publication, leaving a
terminal current-generation session reporting `Playing`/`Paused`. The worker also
published an unconditional coarse `Playing` after a successful `resume`, outside
any boundary.

**Fix (`src/audio/airplay_owntone.rs`, `src/audio/airplay_output.rs`,
`src/audio/airplay_sender.rs`).**

- `SessionInner::transmit_under_boundary` / `transmit_mutation_publishing` hold
  `mutation_lock` across **both** the RPC transmission and the control-state
  publication. `restore` can only latch `terminal` while holding the same lock,
  so a control either publishes before the terminal transition begins or
  observes the latch and publishes nothing. This is the deterministic barrier
  the review required; a bare check before an out-of-lock publish would leave a
  fresh check-to-effect race.
- `activate_and_play` publishes `Playing` through that boundary; its refusal
  path no longer emits a late `Stopped` once terminal.
- `pause` publishes `Paused` through that boundary.
- `run_pump`'s start publication goes through `publish_start_if_live`, which is
  suppressed once terminal.
- `SenderSession` gains `confirm_started`; `run_session_worker` routes its coarse
  `Playing` through the session (`OwnToneSession` implements it via
  `publish_start_if_live`) so the worker boundary respects terminal ordering.

**Regressions.**

- `a_start_publication_after_the_terminal_transition_publishes_nothing` —
  before/after the terminal latch under the boundary.
- `a_pause_after_the_terminal_transition_publishes_no_paused`.
- `the_worker_start_publication_respects_the_terminal_transition`.
- `a_start_publication_cannot_follow_a_concurrent_terminal_restoration` — a
  parked accepted `player/play` races the real `natural_completion`; asserts the
  final cached state is `Stopped`, exactly one `TrackEnded`, and no
  `Playing`/`Paused` follows it.

UI Stop remains non-blocking: no wait was added to the Stop path.

## X2 (P2) — W2 production-path regressions completed

- **Real `OwnToneSession::close` teardown.**
  `a_session_close_settles_a_stalled_play_and_releases_route_and_lock` drives
  the production `close` (not `gate.stop` directly) against a real loopback
  daemon with a real media route and advisory lock, and asserts the route is
  released by identity (route count → 0, custody empty) and the lock is released
  **after** settlement.
- **Real FIFO/EOS completion path.**
  `natural_completion_publishes_exactly_one_track_ended` (exactly one
  `TrackEnded`, after `Stopped`), `natural_completion_failure_is_terminal_and_never_track_ended`,
  and `natural_completion_deadline_miss_is_terminal_and_never_track_ended`
  (real drain deadline; a deadline miss is a timeout error, never a completion).
- **Real `RetainedRecovery` success/release with exact custody.**
  `a_retained_recovery_releases_lock_and_custodied_route_on_settlement` uses a
  hermetic fake **owned** daemon (real process, real `-c <state>/owntone.conf`
  argv binding and ownership record) so `quiesce_daemon` genuinely terminates
  and restarts it; the custodied route is shut down by identity and the
  advisory lock is released only on settlement.
- **Inline `spawn_serialized_recovery` spawn-failure injection.**
  `fail_next_recovery_spawns` forces the inline thread spawn to fail;
  `an_injected_inline_recovery_spawn_failure_hands_off_to_the_supervisor`
  asserts the terminal retained outcome and that the process-global supervisor
  settles it and releases the lock.

## Y1 (P2) — the worker start publication must be inside the terminal boundary

**Review.** `.gc/operations/reviews/refinery-20260915-b739bd7-tr-t3a/corrective-instructions.md`
at rejected head `b739bd7efbc1edbef133c057f84709b3332f1282`, base
`754fc6d8e6c7e7b99b1fe8844b73482f833d668d`.

**Defect.** `confirm_started` returned `Option<PlayerState>`. OwnTone's
implementation published `Playing` under `mutation_lock` and returned
`Some(Playing)`, after which `run_session_worker` stored its own cache and sent
another `Playing` event **outside** the lock. A concurrent `natural_completion`
could latch terminal, restore, and publish `Stopped`/`TrackEnded` in that
window, leaving a current-generation `Playing` after the terminal event.
`natural_completion`'s error/deadline paths also published `Stopped` *before*
`restore` latched terminal, opening the same gap to a queued control.

**Fix (`src/audio/airplay_sender.rs`, `src/audio/airplay_output.rs`,
`src/audio/airplay_owntone.rs`).**

- `SenderSession::confirm_started` now takes the caller's publication as a
  `&mut dyn FnMut(PlayerState)` and runs it at most once while the session's
  terminal-ordering boundary is held. `run_session_worker` passes its cache
  write + state-event send as that closure; a terminal session runs nothing.
- `SessionInner::publish_under_boundary` is the OwnTone implementation (holds
  `mutation_lock`, suppresses on the terminal latch). `activate_and_play` still
  publishes the accepted start through the same boundary.
- `SessionInner::publish_terminal` latches `terminal` and publishes `Stopped`
  as one serialized decision. Every terminal pump path uses it: no bus, failed
  pipeline start, decode error, completion transport loss, drain-deadline miss,
  and failed restore. The successful completion path publishes through the same
  latch after `restore`.

**Regressions.**

- `the_worker_start_publication_respects_the_terminal_transition` (caller
  publication runs/skips under the boundary).
- `the_worker_start_publication_is_atomic_with_the_terminal_transition`
  (publication parked inside the boundary vs. the real `natural_completion`).
- `run_session_worker_publishes_the_start_through_the_session_boundary` drives
  the real worker with a fake session for both the live and terminal cases.

## Y2 (P2) — production-path fixtures completed this leg

- **Real FIFO EOF/drain.** `the_pump_publishes_completion_after_a_real_fifo_drain`
  drives `run_pump` (not `natural_completion` in isolation) with a real decode
  pipeline writing into a real FIFO; the reader observes the writer's EOF and
  only then flips the daemon to `stop`. Asserts PCM written, exactly one
  `TrackEnded` after `Stopped`, daemon restored, cached state terminal.
- **Failed start / no PCM.** `a_failed_start_leaves_the_pump_inert_and_writes_no_pcm`
  drives the real pump with a refused start: the pipeline never starts, the FIFO
  reader sees zero bytes, and no `Playing`/`TrackEnded` is published.
- **Initial volume before first play, through `open()`.** A hermetic *owned*
  fake daemon (real subprocess, `-c <state>/owntone.conf` argv binding,
  ownership record, request recording) lets
  `the_initial_volume_is_applied_before_the_first_play_through_open` drive the
  real `open()`: the `player/volume` PUT is recorded strictly before the first
  `player/play`, and no play appears before `open()` returns. This also
  exercises `verify_owned`/configuration binding (U5 authority path).
- **Stale `Opened` exact custody/route evidence.**
  `a_stale_opened_session_releases_its_route_through_the_worker` drives the real
  `run_session_worker` with a superseded-generation `OpenOutcome::Opened`; the
  route is released by identity (lease gone, custody empty, route count 0).

## Z1/Z2 + AA1/AA2/AA3 (P2) — corrective fixtures

**Reviews.**

- `.gc/operations/reviews/refinery-20260915-0ac3fcc-tr-t3a/corrective-instructions.md`
  (Z1/Z2) at rejected head `0ac3fcc87f608cc05abd055f7d07becae565ce1c`.
- `.gc/operations/reviews/refinery-20260915-0f959938-tr-t3a/corrective-instructions.md`
  (AA1/AA2/AA3) at rejected head `0f95993867a3b0e445a58d90bd6637551805b6ea`.

base `754fc6d8e6c7e7b99b1fe8844b73482f833d668d`.

### AA1 — GStreamer adapter start/Stop, real failed transition and route cleanup

The session is constructible around an **injected pipeline**
(`GstreamerSenderSession::for_test_session`); `resume` always runs the real
`pipeline.set_state(Playing)` effect. The test-only effect override
(`for_test_with_start_effect`) that returned `false` without attempting a
transition was removed, so no regression can substitute a fake for the
production transition. The injected `fakesink name=raop` carries a buffer probe
so consumption is observable without the unavailable production `raopsink`, and
tests drive the real `run_session_worker` through a test sender
(`InjectedPipelineSender`) rather than the seam in isolation.

- `a_gstreamer_session_start_consumes_buffers_and_close_releases_the_route` —
  the authorized start runs the injected pipeline, whose sink observably consumes
  buffers; `close` releases the protected route by identity.
- `a_stop_before_start_refuses_the_gstreamer_start_and_consumes_no_buffers` — a
  Stop taken before any start refuses the effect (no start after Stop), consumes
  no buffers and publishes no `Playing`.
- `a_real_failed_gstreamer_transition_releases_the_route_without_pcm_or_playing`
  — a **real** failed transition: `filesrc` cannot open a missing source, so
  `set_state(Playing)` fails. A synchronous bus recorder proves the transition
  was attempted (state-changed) and failed (error); driven through
  `run_session_worker`, no PCM is consumed, no `Playing` is published, the
  worker reports `Stopped`, and the protected route is released by identity.
- `a_worker_start_through_the_real_gstreamer_adapter_consumes_buffers` — start
  wins the shared boundary through the real worker: buffers are consumed, the
  worker publishes `Playing`, and its teardown releases the route.
- `a_stop_interposed_at_the_real_gstreamer_transition_refuses_the_start` —
  deterministic start-vs-Stop interposition at the **real** transition boundary:
  `set_state(Playing)` is parked inside `filesrc`'s open of a writer-less FIFO,
  the shared `SessionGate` reports the authorized effect in-flight, a Stop then
  wins the boundary, and releasing the parked transition yields a genuinely
  started pipeline whose accepted result is suppressed. No PCM, no `Playing`,
  and the route is released by identity.

No PCM after refusal, no start after Stop, and identity-bound route cleanup are
all asserted. The real registry probe (`GstreamerRaopSender::probe`) remains
fail-closed and untouched.

### AA2 — worker terminal observation asserted while the worker is live

`the_worker_start_publication_is_atomic_with_the_terminal_transition` drives the
**real `run_session_worker`** (via `spawn_test_session_worker`) with a real
`OwnToneSession`. A `#[cfg(test)] SessionProbe` on `SessionInner` (a) parks the
worker's own cache/event publication inside the settlement boundary *before* its
effects run, (b) signals when the terminal `restore` reaches that boundary, and
(c) signals on every live `observe` call. After natural completion the test waits
on the observation signal and asserts the worker's own cache is `Stopped`
**while the worker is still running** — before any `Stop`/teardown can write the
same value and mask a missing refresh. If the loop's cache write were removed the
test times out instead of passing on the teardown store. `Playing` still precedes
exactly one `TrackEnded`, no `Playing`/`Paused` follows it, and the terminal
state is final.

### AA3 — controller failed-live-close replacement under production authority

The prior fixture gave each session a different `session-{index}.lock` while
constructing `SessionInner` directly, permitting a replacement on an instance
production must reject, and asserted replacement usability only as a nonzero
route count.

**Fixture
(`a_failed_live_close_is_replaced_on_a_separate_instance_without_losing_the_recovery_route`).**
Two hermetic, Tributary-owned fake OwnTone instances, each with its own state
dir, ownership record and **production instance lock**
(`state_dir/.tributary-lock`, the single lock production `open` takes). A test
`AirplaySender` builds one real `OwnToneSession` per open, taking that instance
lock. The production controller (`ControllerHarness`, backed by the real
`AirPlayOutput::begin_load`/`close_session`) runs:

1. a live load (#1) on instance A with a real protected-media ticket/route;
2. a `stop` whose worker close fails restoration — the close hands the route to
   **real recovery custody** and retains A's instance lock; the regression
   captures the retained `RetainedRecovery` at that boundary (keyed by
   `api_base`, so no other recovery can steal it);
3. a **same-instance** replacement load (#2) on A: it must **fail closed** while
   recovery owns A's lock — asserted via the published "already using" refusal,
   no live state, and its route released rather than custodied, with A's route
   and lock untouched;
4. a replacement load (#3) on the **separate** instance B: it opens on B's own
   instance lock and plays.

Asserted: UI `stop` returns promptly (never blocks on the failing restoration);
the old exact ticket survives in custody (`is_custodied`, route count 1) and A's
instance lock stays held; the replacement on B installs a live route and is
**observably usable** — it plays, remains `Playing` across A's settlement, and
accepts a real pause/play control; after settlement (clearing A's fail file and
running one real `RecoveryJob::attempt`) only A's route is shut down and only A's
lock is released — B's route and lock are untouched.

## Remaining gaps (recorded honestly, not claimed)

- A live session whose `close` fails restoration is driven through
  `AirPlayOutput` end-to-end (AA3 above), including the production instance-lock
  refusal and a separate-instance replacement.
- The GStreamer adapter's start/Stop, failed-start and route-cleanup paths are
  now covered with an injected pipeline and a real failed transition (AA1
  above); the OwnTone decode pipeline (also GStreamer) remains covered by the
  regressions above.
- U5 authority/configuration certification is not claimed in full here. The
  ownership-binding checks and the initial-volume `open()` fixture exercise the
  paths they assert, but this leg does not blanket-certify Y2 or U5.

## Validation

Run in this worktree (all exit 0):

- `cargo check --all-targets --locked`
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --release -- -D warnings`
- `cargo build --release`
- `cargo test --all-targets` — 1955 unit + 30 packaging, 0 failed
- `cargo test --bin tributary audio::airplay` — 91 passed, 0 failed (includes
  the AA1/AA2/AA3 fixtures above)
- `markdownlint-cli2 v0.23.2` on this file — 0 issues
