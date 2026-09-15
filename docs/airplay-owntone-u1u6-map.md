# tr-t3a — OwnTone AirPlay adapter: U1–U6 corrective map

Maps every blocking finding in the independent refinery review
(`.gc/operations/reviews/refinery-20260915-a0f932e-tr-t3a/corrective-instructions.md`,
rejected head `a0f932e41bad46f1f8a8da400f7ca84d076fab4a`) to its fix and
regression. This map supersedes `docs/airplay-owntone-t1t6-map.md`, whose
T1–T6 dispositions are restated at the end. The design contract is
`docs/airplay-sender-design.md` §4.1/§4.3/§4.4/§9.

Corrective commit on the canonical branch `polecat/tr-t3a` (base
`origin/main` `754fc6d8e6c7e7b99b1fe8844b73482f833d668d`):

| Commit | Scope |
| --- | --- |
| `87ceb31d` | U1–U4 — settlement, live recovery, Stop/start boundary, prepared registration |
| *(this leg)* | U5 — effective process/config/FIFO authority and stable signal delivery; U6 fixtures |

## U1 — failed live restoration left no outstanding-mutation marker

- **Defect:** `SessionInner::restore` cleared a boolean after quiescence and
  then called `restore_daemon` with `?` without re-marking a failed
  restoration. The pump and the control worker accessed that boolean without a
  boundary, so a control could transmit between a restore's quiescence and its
  own effect, and the close path's second restore could succeed and release the
  lock without settling the first timed-out restoration.
- **Fix (`87ceb31d`):** `SessionInner` now carries a settlement boundary
  (`mutation_lock`) and an outstanding **count** (`unsettled`). Every mutating
  RPC (`set_volume`, `pause`, `player/play`, the restoring `player/stop` +
  `outputs/set`) is counted *before* transmission and lowered only on a
  confirmed success; `restore` holds the boundary across quiesce + restore,
  clears the count only after a confirmed quiescence, and re-raises it before
  transmitting the restoring RPCs. A failed restoration therefore retains its
  uncertainty and a later close restore must quiesce first.
- **Regressions:** `a_failed_restoration_keeps_its_uncertainty_across_a_second_restore`,
  `a_failed_control_is_retained_as_outstanding`,
  `restore_fails_closed_while_a_mutation_is_outstanding`,
  `restore_reports_a_failed_restoration`.
- **Remaining:** the delayed-first-restore-then-successful-close-restore and
  concurrent control/EOS fixtures require an HTTP-faithful fake daemon (see
  *Known gaps*).

## U2 — supervisor spawn failure left a permanently unserviced registry

- **Defect:** `RecoverySupervisor::global` discarded `Builder::spawn`'s result
  and permanently initialized the `OnceLock`, so a failed spawn left a registry
  with no worker and no retry; `supervise_job` also retried one job forever, so
  one unavailable instance starved every other.
- **Fix (`87ceb31d`):** the supervisor now services an abstract
  `RecoveryJob` queue in **retry order**: `take_due` waits for the earliest
  due job, attempts it once, and requeues a failure after `RECOVERY_POLL`.
  Worker liveness is proven (`ensure_worker` returns whether a spawn actually
  succeeded), a `WorkerAlive` guard clears the flag on exit so a dead worker is
  restarted, and worker creation is retried (`register` bounded retries +
  `global()`/`ensure_worker_if_pending`) while the job — and its advisory lock —
  stays queued.
- **Regressions:** `supervisor_preserves_a_job_when_its_worker_cannot_start`
  (injected spawn failure; the job is preserved and settles once creation
  succeeds), `supervisor_services_two_independent_jobs_fairly` (two independent
  non-settleable jobs both reach release).
- **Remaining:** injecting the *inline* `airplay-owntone-recovery` thread-spawn
  failure and observing eventual lock/custody release through the real
  `RetainedRecovery` path still needs a daemon harness (see *Known gaps*).

## U3 — cancellation remained a check-to-effect race

- **Defect:** `AirPlayOutput::close_session` only set an atomic cancel flag;
  OwnTone's `resume` released the activation mutex before a separate
  `player/play`, and the decode pump released before a separate
  `pipeline.set_state(Playing)`. `activate` also marked accepted before the
  play RPC, so a failed play could still start the pump and publish `Playing`.
- **Fix (`87ceb31d`):** a shared `SessionGate` is threaded through
  `SenderOpenContext`, `LoadController`, and both adapters. `close_session`
  stops the gate; OwnTone transmits `player/play` **under** the gate and only
  sets `accepted` after the RPC succeeds; GStreamer authorizes and performs the
  pipeline start under the same gate. A Stop either refuses the start before it
  runs or follows a transmitted effect that the session's teardown settles.
- **Regressions:** `session_gate_serializes_stop_and_start`,
  `a_stop_before_start_refuses_the_effect`,
  `a_failed_initial_play_publishes_no_playing`,
  `activation_refuses_once_the_load_is_cancelled`.
- **Remaining:** a GStreamer-pipeline Stop barrier and a real no-PCM/no-TrackEnded
  assertion need a pipeline/daemon harness (see *Known gaps*).

## U4 — prepared tickets had no identity-bound registration

- **Defect:** `begin_open` ran on the spawned worker after preparation, and
  registration compared a `current_open` slot that `retire_active_locked`
  cleared, so a replacement preparation could revoke a prepared route before a
  delayed worker installed itself into the cleared slot.
- **Fix (`87ceb31d`):** `PreparedGstreamerMedia` now carries the preparation
  generation it was minted under. `register_in_flight_cancel` is bound to that
  generation (`Arc::ptr_eq` with the proxy's current generation) instead of a
  clearable slot, and the load path takes the registration **before scheduling
  the worker**. A superseded prepared load registers nothing; a replacement
  preparation that lands after registration sees the load in-flight and
  preserves its route in custody.
- **Regressions:** `a_superseded_registration_installs_nothing`,
  `a_stale_registration_cannot_overwrite_a_newer_authorization`,
  `replacement_preparation_custodies_an_inflight_route_instead_of_revoking`,
  `take_and_release_is_identity_bound_for_a_stale_ticket`,
  `delayed_typed_startup_cannot_replace_a_newer_load`.
- **Remaining:** replacement during a failed live close and a stale `Opened`
  completion with route/custody counts still need the live-session harness (see
  *Known gaps*).

## U5 — config filename matching was not dedicated process/config/pipe binding

- **Defect:** `cmdline_binds_state_dir` accepted **any** whitespace-delimited
  argument equal to the state directory or its lexically normalized
  `owntone.conf`; it never required the effective configuration option,
  canonicalized through symlinks, read the config, or tied it to
  `OwnToneConfig.pipe_path`. A separate `state_dir` argument plus an unrelated
  effective config passed, and a symlinked expected name passed. Numeric-pid
  delivery still had a check-to-signal window.
- **Fix (this leg):**
  - `ListenerProcess` keeps the NUL-separated **argv**, not a whitespace-joined
    string, so argument boundaries cannot be forged.
  - `effective_config_argument` accepts only `-c <file>`, `--config <file>`,
    `--config=<file>`, or attached `-c<file>`, and fails closed on zero, empty,
    or multiple effective configurations. A bare path argument is not accepted.
  - `cmdline_binds_instance` canonicalizes both the state directory and the
    effective config, requires the config to equal
    `<canonical state>/owntone.conf`, requires that name to be a **regular
    file** (not a symlink), and then reads it and requires its `pipe_path`
    directive to resolve to `OwnToneConfig.pipe_path` (`config_binds_pipe`).
  - `SignalHandle`: on Linux signals are delivered through a **`pidfd`**
    (`pidfd_open` + `pidfd_send_signal`), which names the exact process and
    cannot reach a recycled pid — no check-to-signal window; the handle also
    re-proves the observed start time on open. Other Unix targets retain the
    numeric path with an immediate start-time re-check.
- **Regressions:** `cmdline_binding_requires_the_effective_configuration_and_pipe`
  (effective option, `--config=` form, in-tree `..`, bare path, foreign config,
  sibling-prefix collision, out-of-tree `..`, non-canonical config, ambiguity,
  symlinked config, different FIFO),
  `signal_and_wait_stops_a_child_process`,
  `signal_and_wait_refuses_a_replaced_identity`,
  `signal_and_wait_treats_a_gone_process_as_quiesced`,
  `verify_daemon_process_refuses_a_foreign_listener`.
- **Note:** `config_binds_pipe` reads the documented OwnTone `pipe_path`
  directive. If the pinned 29.3 key differs, this is the one place to adjust.

## U6 — required behavioral evidence

- **Fixed:** `a_stalled_owntone_endpoint_does_not_block_load_or_stop` is now a
  **production-path** fixture (Linux). It compiles a hermetic fake daemon at
  test time whose executable and `-c <state>/owntone.conf` launch make
  `verify_daemon_process`/`cmdline_binds_instance` pass, so the load genuinely
  reaches the `/api/config` handshake and stalls there; `load_uri`/`stop` are
  asserted non-blocking. The previous fixture's dummy executable (which never
  passed ownership) and dropped accepted socket are gone.
- **Preserved:** all prior F1–F5 / R1–R5 / S1–S7 / T1–T6 assertions remain;
  `docs/airplay-owntone-f1f5-map.md`, `-r1r5-map.md`, `-s1s7-map.md`, and
  `-t1t6-map.md` still stand.

## Known gaps (not closed at this head)

These are recorded honestly rather than claimed as resolved. Every one needs an
HTTP-faithful fake OwnTone daemon (the stalled fixture's helper extended to
serve `/api/config`, `/api/outputs`, `/api/player`, `/api/queue`) plus FIFO
control:

1. U1 delayed-first-restore-then-successful-close-restore; concurrent
   control/EOS with no late effects after release.
2. U2 inline recovery spawn-failure injection through the real
   `RetainedRecovery` path with observed eventual lock/custody release.
3. U3 deterministic GStreamer Stop barrier before pipeline start; a real
   no-`Playing`/no-PCM/no-`TrackEnded` assertion for a failed start.
4. U4 replacement during failed live close; stale `Opened` completion with
   exact route/custody counts.
5. U6 real FIFO EOF/drain/deadline/exactly-once F4 events, and initial-volume
   daemon-call ordering (currently the numeric helper is tested).

## Validation

Run in this worktree against the pushed branch:

- `cargo check --all-targets --locked` — exit 0
- `cargo fmt --check` — exit 0
- `cargo clippy --all-targets -- -D warnings` — exit 0
- `cargo clippy --release -- -D warnings` — exit 0
- `cargo build --release` — exit 0
- `cargo test --all-targets` — 1926 + 30 + 20 + 1 passed, 0 failed, exit 0
