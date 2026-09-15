# tr-t3a — OwnTone AirPlay adapter: T1–T6 corrective map

Maps every blocking finding in the independent refinery review
(`.gc/operations/reviews/refinery-20260915T0330Z-tr-t3a/corrective-instructions.md`,
rejected head `b914497c15a2caad5fdd2a5e911deed40cc1a807`) to its fix and
validation. This map supersedes `docs/airplay-owntone-s1s7-map.md`, whose
S1–S7 / R1–R5 / F1–F5 dispositions are restated at the end. The design contract
is `docs/airplay-sender-design.md` §4.1/§4.3/§4.4/§9.

Corrective commits on the canonical branch `polecat/tr-t3a`:

| Commit | Scope |
| --- | --- |
| `7ada9b80` | T5, T6 — process identity, endpoint family, Linux test gating |
| `144b09b5` | T1, T2, T3 — mutation settlement, recovery supervisor, Stop/play boundary |
| `83e501eb` | T4 — ticket lifecycle bound to the live load |

## T1 / S3 / R2 / F3 — P1: newly outstanding mutations still escape quiescence

- **Defect:** `settle_restore` discarded a failed `quiesce_daemon` and could
  accept a later compensation as proof of settlement; `spawn_serialized_recovery`
  resolved `RestorationFailed` from a `quiesced` value that predated a failed
  restoration attempt; the initial `set_volume` failure unwound as `unsettled =
  false`; and live resume/pause/volume errors were never recorded, so a close
  could accept a later successful restore after a timed-out control RPC.
- **Fix (`144b09b5`):**
  - `settle_restore` performs one clean restoration first; after any failed
    restoration it quiesces before compensating and returns `false` if
    quiescence cannot be established — a later compensation is never accepted as
    proof of settlement.
  - `spawn_serialized_recovery` tracks `settled` (a successful quiescence with no
    subsequent failed restoration attempt). A failed restoration attempt clears
    `settled`, so a deadline crossing resolves `Retained`, not
    `RestorationFailed`.
  - The initial `set_volume` failure now sets `unsettled = true`, matching
    `set_outputs`/`clear_queue`.
  - `SessionInner` carries `mutation_outstanding`; `set_volume`, `pause` and the
    failed `resume` play RPC mark it, and `restore` quiesces before releasing and
    fails closed if quiescence cannot be established.
- **Regression:** `restore_fails_closed_while_a_mutation_is_outstanding`.

## T2 / S3 — P1: Retained leaks the lock without an executable recovery owner

- **Defect:** `mem::forget(lock)` at the two `Retained` resolutions leaked the
  only advisory-lock descriptor and left the route custodied with no registered
  releaser — a permanent failed state.
- **Fix (`144b09b5`):** replaced both leaks with a process-global
  `RecoverySupervisor` (lazy worker thread + condvar queue). On a failed
  quiescence or an inline spawn failure the lock, client, recorded state and
  route are registered; the supervisor keeps the lock held and retries
  quiescence + restoration until settlement, then releases the route by identity
  and drops the lock — a live recovery owner until proven settlement.
- **Regression:** the supervisor is exercised only on the retention path;
  `retained_recovery_outcome_is_terminal_and_distinct` still covers the terminal
  contract. (No injected quiesce-failure fixture yet — see *Known gaps*.)

## T3 / S4 / R4 — P1: Stop still does not serialize with activation

- **Defect:** `AirPlayOutput::close_session` only set the cancel token and sent
  `Stop`; `SessionInner::activate` never consulted that token, so a Stop after
  the worker's currentness check but before `resume` still sent `player/play`.
  `resume` also ignored a failed play RPC while the worker published `Playing`
  unconditionally.
- **Fix (`144b09b5`):**
  - `activate` re-checks the load's cancellation currency (`self.cancel`) inside
    the activation mutex and marks the boundary cancelled, so a Stop that raced
    the currentness check refuses activation before any `player/play`.
  - `resume` re-checks cancellation immediately before transmitting play; a
    transmitted play races the teardown's restoration `player/stop`, which
    compensates it.
  - `SenderSession::resume` now returns `bool`; the worker reports the real
    outcome (`Stopped` on a failed/cancelled start) instead of publishing
    `Playing` unconditionally, and the OwnTone adapter surfaces the failed-play
    error.
- **Regression:** `activation_refuses_once_the_load_is_cancelled`.

## T4 / S5 / F3 — P1: established-session replacement revokes before close recovery

- **Defect:** `begin_open` unconditionally installed `current_open`, so a stale
  worker could overwrite a newer load's authorization. After the open,
  `drop(registration)` left the live ticket active with no in-flight entry, so a
  replacement preparation revoked the live route before the session's close
  recovered it. The GStreamer close/EOS/error paths used `revoke_if_current`,
  which never removed a recovery-custody entry, leaking superseded servers.
- **Fix (`83e501eb`):**
  - `begin_open` is scheduling-ordered: only an id at least as new as the
    current authorization installs, so a stale worker cannot overwrite a newer
    load's identity.
  - The in-flight registration now outlives the open. While the session is live
    the load stays counted in-flight, so a replacement preparation preserves the
    live route in recovery custody (and cancels its handle) instead of revoking
    it before close.
  - `GstreamerSenderSession::close` and the EOS/error bus branches use the
    identity-bound `take_and_release`, which removes a recovery-custody entry as
    well as the active lease and revokes the route.
- **Regression:** `a_stale_begin_open_cannot_overwrite_a_newer_authorization`;
  the existing `replacement_preparation_custodies_an_inflight_route_instead_of_revoking`
  and `recovery_custody_preserves_the_route_and_releases_by_identity` cover the
  custody/release contract.
- **Remaining:** the pre-registration hole is closed for the authorization slot
  and the live route is preserved, but the requested *replacement during failed
  live close* and *stale Opened completion on both adapters* fault-injection
  fixtures are not yet added (see *Known gaps*).

## T5 / S2 / R5 — P1: signal/configuration authority remains incomplete

- **Defect:** `signal_and_wait` received only a numeric pid and checked identity
  once before entering; a `/proc` read failure was reported as process absence;
  `cmdline_binds_state_dir` accepted `state/../foreign.conf` and any config
  beneath the state directory; `localhost` matched either loopback family while
  HTTP may have dialled the other.
- **Fix (`7ada9b80`):**
  - `observe_process` distinguishes `Gone` from `Unobserved`; an unreadable
    `/proc` state is an error, never quiescence.
  - `signal_and_wait` takes the `ProcessIdentity` and re-verifies pid/start-time
    immediately before `TERM` and again before the later `SIGKILL`.
  - `cmdline_binds_state_dir` lexically normalizes every candidate and requires
    the canonical `owntone.conf`; a `..` traversal out of the state directory
    and any unrelated config beneath it are refused.
  - `verify_loopback` requires a literal loopback address, so the kernel listener
    match and the HTTP dial target the same family; ambiguous `localhost` is
    refused.
- **Regressions:** `signal_and_wait_refuses_a_replaced_identity`,
  `cmdline_binding_requires_the_exact_state_directory` (traversal, unrelated
  config, in-tree `..`), `loopback_verification_accepts_only_loopback_endpoints`.
- **Remaining:** canonical pipe binding is asserted through the state directory
  and canonical config path; a distinct IPv4/IPv6 `localhost` listener fixture
  is not yet added (the ambiguous name is now refused).

## T6 / S1 — P1: Linux-only process tests still run on macOS

- **Defect:** `listener_process_resolves_the_process_bound_to_a_port`
  unconditionally expected `/proc` enumeration, and the signal tests treated an
  unreadable `/proc` state as gone, so they did not prove exit on macOS.
- **Fix (`7ada9b80`):** `signal_and_wait_stops_a_child_process`,
  `signal_and_wait_treats_a_gone_process_as_quiesced`,
  `signal_and_wait_refuses_a_replaced_identity`,
  `listener_process_resolves_the_process_bound_to_a_port` and
  `verify_daemon_process_refuses_a_foreign_listener` are gated
  `#[cfg(target_os = "linux")]`, with
  `process_resolution_is_unsupported_off_linux` providing explicit
  unsupported-platform coverage on other targets.

## Prior disposition and required behavioral coverage

All prior F1–F5 / R1–R5 / S1–S7 behavioral assertions are preserved; the maps
`docs/airplay-owntone-f1f5-map.md`, `docs/airplay-owntone-r1r5-map.md` and
`docs/airplay-owntone-s1s7-map.md` still stand. F4 (`pause` is not completion)
and the affected regressions are unchanged.

## Known gaps (not yet closed at this head)

These are recorded honestly for the next corrective leg rather than claimed as
resolved:

1. **Fault-injection fixtures.** The refinery requested delayed-mutation
   fixtures (open volume, live play, restoration PUT, failed quiescence,
   deadline crossing), recovery spawn-failure and subsequent-recovery tests, a
   deterministic Stop barrier after the currentness check with no stale
   play/PCM/start event, and replacement-during-failed-close / stale-`Opened`
   tests on both adapters with route-count and custody-emptiness assertions.
   The production contracts for T1–T6 are implemented; these injected fixtures
   are only partly present.
2. **Distinct IPv4/IPv6 `localhost` listeners.** The ambiguity is removed by
   requiring a literal address; the two-listener fixture is not added.
3. **R1 stalled-endpoint test.** The dummy executable and listener are still
   different processes, and `listener.accept().is_ok()` still drops the accepted
   stream; the `/api/config`-receipt assertion is unchanged from the prior leg.

## Validation at this head

Configured Linux gates, run in this worktree against the pushed branch:

- `cargo check --all-targets --locked` — exit 0
- `cargo fmt --check` — exit 0
- `cargo clippy --all-targets -- -D warnings` — exit 0
- `cargo clippy --release -- -D warnings` — exit 0
- `cargo build --release` — exit 0
- `cargo test --all-targets` — 20 + 1 + 1919 + 30 passed, 0 failed

Runtime tests are not skipped by this rig (`run_tests` is false in the formula
vars, but the full `cargo test --all-targets` suite was run locally and passed).
No Windows/macOS build or physical-device validation was performed in this seat;
the `not(unix)` module path and the `not(target_os = "linux")` test arm are
asserted by construction.
