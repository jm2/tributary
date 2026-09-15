# tr-t3a — OwnTone AirPlay adapter: S1–S7 corrective map

Maps every blocking finding in the independent refinery review
(`.gc/operations/reviews/refinery-20260915T0200Z-tr-t3a/corrective-instructions.md`,
head `ab06dc3382b0055a48b7c33b8f9c92731b760e70`) to its fix and validation.
This map supersedes `docs/airplay-owntone-r1r5-map.md`, whose R1–R5 / F1–F5
dispositions are restated at the end. The design contract is
`docs/airplay-sender-design.md` §4.1/§4.3/§4.4/§9.

## S1 / F1 — P1: new controller regression broke non-Unix test compilation

- **Defect:** `src/audio/airplay_output.rs` unconditionally compiled
  `a_stalled_owntone_endpoint_does_not_block_load_or_stop`, which calls
  `airplay_owntone::test_owned_sender`. On `not(unix)`, `mod.rs` selects
  `airplay_owntone_unsupported.rs`, which defines no such function, so Windows
  `--all-targets`/test compilation had an unresolved function.
- **Fix:** the Unix-only regression is now gated with `#[cfg(unix)]`. A
  `#[cfg(not(unix))]` counterpart (`an_unsupported_platform_owntone_sender_refuses_the_load`)
  exercises the unsupported shim: selection is recognized and the sender
  refuses explicitly, so no load is misreported as an OwnTone session.
- **Validation:** the Unix test still runs here; the non-Unix arm compiles
  against the shim. No Windows toolchain is available in this seat, so Windows
  compilation is asserted by construction (the `not(unix)` module path is the
  same gate `mod.rs` already uses).

## S2 / R5 — P1: recovery could terminate a foreign OwnTone instance

- **Defect:** `quiesce_daemon` resolved the listener but checked only
  `same_binary` before `terminate_process`; `listening_inodes` matched only the
  port, ignoring the bound address; `cmdline_binds_state_dir` used substring
  matching (so `/run/tributary-other` matched `/run/tributary`); and no stable
  process identity was re-verified before signalling.
- **Fix:**
  - `listening_inodes` / `listener_process` now match the configured loopback
    **address and port**. `expected_local_addrs` renders `/proc/net/tcp{,6}`
    local addresses in the kernel's little-endian form (IPv4 `u32`, IPv6 four
    `u32` words) for `127.0.0.1`, `::1` and `localhost`.
  - `cmdline_binds_state_dir` is path-component exact (`arg == state_dir ||
    arg.starts_with(state_dir)`), so a sibling directory is rejected.
  - `quiesce_daemon` requires the full `process_is_owned` binding (binary +
    state-directory binding), not just the binary.
  - `ListenerProcess` now carries the kernel start time (field 22 of
    `/proc/<pid>/stat`); `verify_signal_target` re-resolves the pid immediately
    before signalling and requires the same pid+start-time identity and the
    full ownership binding.
- **Tests:** `listening_inodes_parses_the_proc_net_tcp_table` (address+port,
  same-port/different-address row, non-LISTEN row),
  `expected_local_addrs_use_the_kernel_byte_order`,
  `cmdline_binding_requires_the_exact_state_directory` (added sibling-prefix
  rejection), `verify_daemon_process_refuses_a_foreign_listener`.

## S3 / R2 — P1: recovery released without proven quiescence

- **Defect:** `terminate_process` ignored signal errors and returned `Ok`
  immediately after `SIGKILL` without observing exit; `wait_for_owned_listener`
  could accept the same still-live process; `spawn_serialized_recovery`
  resolved `RestorationFailed` at the deadline even when `quiesced == false`,
  and a spawn failure dropped the captured lock and resolved immediately; and
  `settle_restore` could itself issue a timed-out restoring `PUT` and then
  accept a later success for that newly outstanding mutation.
- **Fix:**
  - `signal_and_wait` reports signal errors (`ESRCH` is treated as already
    gone), escalates to `SIGKILL` at the deadline, and **confirms** exit within
    `QUIESCE_KILL_DEADLINE`; a process that survives `SIGKILL` is an error.
    `terminate_process` re-verifies authority/identity first.
  - `wait_for_owned_listener` takes the terminated identity and will not accept
    the same still-live process as a "restart".
  - `settle_restore` quiesces the daemon after every failed restoration step
    before retrying, so a timed-out restoring `PUT` cannot be retracted by a
    later compensation.
  - `spawn_serialized_recovery` only resolves `Restored` after confirmed
    quiescence; `RestorationFailed` is only reported when quiescence was
    established; otherwise the new terminal `RecoveryOutcome::Retained` is
    reported and the advisory lock is retained (the descriptor is leaked so the
    flock stays held) with the route left custodied for the supervisor. A spawn
    failure likewise retains the lock and reports `Retained` rather than
    dropping ownership.
  - The load path (`run_session_worker`) does not release the route on
    `Retained`.
- **Tests:** `signal_and_wait_stops_a_child_process`,
  `signal_and_wait_treats_a_gone_process_as_quiesced`,
  `retained_recovery_outcome_is_terminal_and_distinct`. Delayed-mutating-response
  and spawn-failure behavior is exercised at the code boundary (the fork
  points above); a fault-injecting daemon is out of this adapter's unit scope.

## S4 / R4 — P1: Stop could still race the unconditional activation RPC

- **Defect:** `run_session_worker` checked currentness once, then called
  `resume`; `OwnToneSession::resume` set `activated` and unconditionally sent
  `player/play`; the pump had a check-to-start window after
  `wait_for_activation`. A Stop between check and play could transmit a stale
  daemon play (and the pump could start PCM) for a cancelled load.
- **Fix:** `SessionInner.activated: AtomicBool` is replaced by a single
  `Mutex<ActivationState>` shared by acceptance, cancellation and the pump.
  `activate()` accepts only while running and not cancelled
  (`activation_decide`); `resume` transmits `player/play` only on an accepted
  activation; `cancel_activation()` (called by `OwnToneSession::close` before
  teardown) wins the boundary so a late acceptance is refused; and `run_pump`
  performs a final serialized `activation_live()` check before any pipeline
  start. Teardown still runs the restoration contract, so a `play` transmitted
  just before a cancellation is covered by restore.
- **Tests:** `activation_and_cancellation_share_a_serialized_boundary`,
  `activation_gate_releases_only_on_accepted_activation` (now reads the shared
  `ActivationState`).

## S5 / R3 / F3 — P1: custody revocation gap and retained-entry leak

- **Defect:** `run_session_worker` dropped the in-flight registration before
  moving the ticket into custody, so a replacement preparation in the interval
  could revoke the active route while recovery was outstanding. Established
  session close recovery retained only an `Arc` in `route` and never moved the
  ticket off the active lease. `SessionInner::restore` and recovery success
  called `revoke_if_current`, whose non-active branch revokes the route but
  leaves a custody entry (and its server) alive.
- **Fix:**
  - `open` now builds a `CustodyHandoff` from the context and `recovery_pending`
    moves the ticket into keyed custody **before** constructing
    `SenderError::RecoveryPending`, so by the time the load path sees the
    outcome the route is off the active lease. The serialized recovery receives
    the route and releases it at its terminal disposition.
  - `OwnToneSession::close` moves the ticket to custody before starting
    serialized recovery, so a replacement preparation cannot revoke it in the
    interval.
  - `GstreamerMediaProxy::take_and_release` is the single identity-bound release
    primitive and now also revokes a ticket that is in neither the active lease
    nor custody; `SessionInner::restore` and the recovery success path use it
    instead of `revoke_if_current`, so terminal cleanup empties custody.
  - The `Retained` recovery outcome leaves custody in place for the supervisor.
- **Tests:** `recovery_custody_preserves_the_route_and_releases_by_identity`
  (now asserts empty active lease and empty custody after terminal cleanup),
  `take_and_release_is_identity_bound_for_a_stale_ticket`, and the existing
  `replacement_preparation_custodies_an_inflight_route_instead_of_revoking`.
  The boundary move itself is exercised through `CustodyHandoff` construction
  in the open path.

## S6 — P1: fail-closed probe ran after media preparation

- **Defect:** `load_uri`/`load_resolved`/`load_local` invoked
  `media_proxy.prepare*` before `begin_load` ran `sender.probe`, so an
  unavailable sender could still mint a loopback ticket or open local media;
  and `start_session_worker` discarded spawn errors with `.ok()`, leaving the
  route and a `Buffering` state.
- **Fix:** `begin_load` now takes a preparation closure and runs `probe()`
  **first**; preparation only runs after a successful probe. `MediaProxy`
  preparation for a load then happens inside that closure.
  `start_session_worker` captures a ticket handle and, on a worker-spawn
  failure, releases the prepared route via `take_and_release` and reports a
  `Stopped` failure.
- **Tests:** `a_failing_probe_never_prepares_or_mints_a_route` (valid runtime,
  protected URI, failing sender: the preparation closure does not run and no
  active lease/custody is created) and the existing failing-probe tests.

## S7 — P2: initial OwnTone playback ignored the user's volume

- **Defect:** `SenderOpenContext.volume` was consumed by the GStreamer adapter
  but never by OwnTone; the daemon started at its prior volume.
- **Fix:** `open` applies the user's volume (`volume_percent`, clamped
  `0.0–1.0 → 0–100`) via `PUT /api/player/volume` in the mutation phase,
  before any activation, and treats a failed volume RPC as a failure that
  unwinds rather than swallowing it.
- **Test:** `volume_percent_maps_and_clamps_the_slider`. The ordering
  (volume before `player/play`) is a code-level guarantee in `open`; a live
  daemon initialized at full volume is out of this adapter's unit scope.

## Prior finding disposition (R1–R5 / F1–F5)

- **R1** — unchanged and preserved: the blocking handshake stays on the load
  worker; `probe` remains the local configuration check.
- **R2** — strengthened by S2/S3: full authority + stable identity before
  signalling, confirmed exit after `SIGKILL`, and quiescence required before a
  terminal recovery outcome.
- **R3** — strengthened by S5: `restore` uses the single identity-bound
  `take_and_release`, and the recovery success path releases custody.
- **R4** — strengthened by S4: acceptance/cancellation now share one serialized
  boundary and the pump re-checks before start.
- **R5** — strengthened by S2: endpoint matching is address+port and the
  command-line binding is path-component exact.
- **F1** — S1 closes the non-Unix test-compilation regression.
- **F2, F3** — S2/S3/S5 close the residual quiescence, custody-move and
  release-on-all-paths gaps.
- **F4** — preserved: the FIFO write end still closes before daemon completion
  polling and `pause` is not completion
  (`completion_requires_stop_and_never_accepts_pause`).
- **F5** — S2 closes the endpoint/identity binding.

## Gates at this head

- `cargo check --all-targets --locked`
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --release -- -D warnings`
- `cargo build --release`
- `cargo test --all-targets`
