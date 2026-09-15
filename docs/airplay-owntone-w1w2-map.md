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

## Remaining gaps (recorded honestly, not claimed)

The following V1–V3/W2 fixtures still lack a production-path harness and are
**not** claimed as resolved by this leg:

1. A **full-session** replacement/close fixture that drives
   `AirPlayOutput::stop`/replacement through `run_session_worker` and a real
   `OwnToneSession::close` (route/custody release observed end to end). The
   terminal-settlement contract itself is covered at the `SessionInner`
   boundary with a real HTTP daemon above.
2. Real FIFO EOF/drain/deadline/exactly-once `TrackEnded` regressions and
   initial-volume daemon-call ordering.
3. Deterministic GStreamer pipeline start/Stop barrier and a real
   no-`Playing`/no-PCM failed-start assertion.
4. Inline `airplay-owntone-recovery` thread-spawn-failure injection through
   `spawn_serialized_recovery` itself (only the supervisor-worker spawn is
   injectable today); `RetainedRecovery` retention is exercised directly.

## Validation

Run in this worktree against the pushed branch (all exit 0):

- `cargo check --all-targets --locked`
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --release -- -D warnings`
- `cargo build --release`
- `cargo test --all-targets`
