# tr-t3a — OwnTone AirPlay adapter: F1–F5 corrective map

Maps every blocking finding in the independent refinery review
(`.gc/operations/reviews/refinery-20260914-tr-t3a/corrective-instructions.md`,
head `5b0b11ec`) to the fix at this head. The design contract is
`docs/airplay-sender-design.md` §4.1/§4.3/§4.4 and §9.

## F1 — P1: unsupported platforms no longer compile

- **Fix:** `src/audio/mod.rs` no longer declares `airplay_owntone`
  unconditionally. On `cfg(unix)` the real adapter compiles; on `not(unix)` a
  fail-closed shim at `src/audio/airplay_owntone_unsupported.rs` is compiled in
  its place. The shim exposes the same `OwnToneSender` shape
  (`selected`/`from_env`/`probe`/`open_session`) and refuses with localized
  guidance, so an explicit `TRIBUTARY_AIRPLAY_SENDER=owntone` never silently
  falls back to another sender (design §4.4, §9 platform scope). The Unix-only
  imports (`std::os::fd`, `std::os::unix`, `rustix`) are no longer reachable on
  Windows because the module that contains them is not compiled there.
- **Regression:** the shim keeps the exact-selection and fail-closed-probe
  assertions (`airplay_owntone_unsupported.rs` tests).

## F2 — P1: opening and controls block GTK and cannot be cancelled

- **Fix:** `AirPlayOutput` now runs each load on a dedicated worker thread
  (`run_session_worker`) with a `LoadController` command channel. The blocking
  `open_session` — and every subsequent pause/resume/volume/close RPC and pump
  join — runs on the worker; UI methods only send commands and read cached
  state/position, so GTK is never blocked (review F2).
- **Cancellation:** `OpenCancel` is shared between the controller and the
  worker. `close_session`, Stop, and replacement all cancel it and signal the
  worker. `open()` checks it between every blocking step, and the FIFO wait
  (`open_pipe_write`) is raced against it. A stale open (generation superseded
  or cancelled after negotiation) closes its session without starting playback.
- **Keyed registration:** `GstreamerMediaProxy` now has `begin_open`,
  `register_in_flight_cancel` + `InFlightCancelRegistration` (keyed by
  per-load `open_id`, superseded-checked under the state lock), and
  `retire_active_locked`, which preserves an in-flight route in keyed custody
  and cancels its registered handle instead of revoking it. A guard's `Drop`
  removes only its own keyed entry.
- **Tests:** `replacement_preparation_custodies_an_inflight_route_instead_of_revoking`,
  `a_superseded_registration_installs_nothing`.

## F3 — P1: failed mutations erase recovery evidence and release ownership

- **Fix:** `SessionInner::restore` and the new `restore_daemon` return/leave the
  incomplete-takeover record **in place** on any failed restoration step and do
  not revoke the route; the record is cleared only after every step succeeds.
- **Recovery:** a failure or cancellation after the first mutating RPC unwinds
  through `fail_outcome`/`cancel_outcome`. A clean "settle" (`settle_restore`,
  bounded by `CLEANUP_DEADLINE`) releases ownership and returns the original
  outcome (`Cancelled` for a cancellation). An unsettled mutation or a failed
  restoration produces `SenderError::RecoveryPending` carrying a
  `RecoveryCompletion`; the serialized recovery (`recovery_pending`) holds the
  advisory lock and retries restoration until `RECOVERY_DEADLINE`, then resolves
  `Restored` or `RestorationFailed` with the record retained for the supervisor.
- **Custody on the load path:** the worker moves the media ticket into keyed
  recovery custody and releases it only after the completion handle resolves;
  it never releases on receipt.
- **Worker-spawn failure after takeover:** the pump-spawn failure path reclaims
  the session state and runs the same unwind instead of leaking a
  half-taken-over daemon.
- **Tests:** `failed_restoration_preserves_the_takeover_record`,
  `recovery_custody_preserves_the_route_and_releases_by_identity`.
- **Known limitation:** the adapter cannot itself terminate/restart the daemon,
  so the "restart" half of settle-or-restart is delegated to the supervisor via
  the retained record; restoration is retried up to the recovery deadline.

## F4 — P1: EOF is withheld while waiting for completion

- **Fix:** `run_pump` owns the FIFO write end in an `Option` and drops it
  **before** `natural_completion` waits, so the daemon's reader observes EOF and
  the item can finish naturally. `natural_completion` now requires the daemon to
  report `stop`: `pause` is no longer accepted as completion, and a drain
  deadline miss or transport loss still ends as `Error` + `Stopped` with no
  `TrackEnded`.

## F5 — P1: a marker file does not bind the configured API to the owned daemon

- **Fix:** the constant ownership marker is replaced by a JSON
  `OwnershipRecord` (token + `api_base` + `pipe_path` + `state_dir` + `binary`).
  `verify_owned` refuses unless every field matches the configured values, so a
  valid token paired with a foreign API endpoint (or pipe/state/binary) is
  refused before any receiver state is read or mutated.
- **Tests:** `ownership_record_binds_endpoint_pipe_state_and_binary`,
  `ownership_record_rejects_a_foreign_token_and_a_missing_record`.

## Gates at this head

- `cargo check --all-targets --locked`
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --release -- -D warnings`
- `cargo test --all-targets` (1899 lib + 20 + 1 + 30, 0 failed)
