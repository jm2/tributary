# tr-t3a — OwnTone AirPlay adapter: R1–R5 corrective map

Maps every blocking finding in the independent refinery re-review
(`.gc/operations/reviews/refinery-20260915-tr-t3a/corrective-instructions.md`,
head `4bc5d438`) to its fix and validation. This map supersedes
`docs/airplay-owntone-f1f5-map.md`, whose F1–F5 dispositions are restated at the
end. The design contract is `docs/airplay-sender-design.md` §4.1/§4.3/§4.4/§9.

Corrective commits on `polecat/tr-t3a`:

- `f70b1893` — R1, R4
- `ad9b5185` — R2, R3, R5
- the commit carrying this document — F4 coverage

## R1 / F2 — P1: availability probe still blocked GTK

- **Fix:** `OwnToneSender::probe` is now only the local, fail-closed
  configuration check (platform, ownership record, binary, pipe). The blocking
  daemon reachability/version handshake moved to `check_daemon_health`, called
  by `open` on the load worker — before the advisory lock and before any
  receiver state is read or mutated. A slow or stalled daemon can no longer
  freeze the UI before the worker starts.
- **Regression:** `a_stalled_owntone_endpoint_does_not_block_load_or_stop`
  drives a real `OwnToneSender` against a listener that accepts and never
  answers and asserts `load_uri` and a following `stop` both return promptly.

## R2 / F3 — P1: recovery declared success without server-side quiescence

- **Fix:** the shared serialized recovery (`spawn_serialized_recovery`) now
  performs bounded quiescence *before* any restoration attempt.
  `quiesce_daemon` resolves the owned listener out of band, terminates it
  (`SIGTERM`, escalating to `SIGKILL` at `QUIESCE_TERMINATE_DEADLINE`), then
  brings the instance back through the installation record's supervisor
  restart command (`OwnershipRecord.restart_command`, a new optional field) or
  waits (`QUIESCE_RESTART_DEADLINE`) for the owned listener to return. Only
  then does `restore_daemon` run, so no old-generation `outputs/set`,
  `queue/add` or `player/play` can land after `Restored`. The advisory lock and
  the route stay held until the terminal outcome.
- **Scope:** the open path's `unsettled` branch (`fail_outcome` /
  `cancel_outcome` → `recovery_pending`) and the new close-failure path both
  route through the quiescing recovery. The `unsettled == false` paths keep the
  existing `settle_restore` compensation, which is correct: no mutating RPC was
  left in flight, so there is nothing to quiesce.
- `rustix`'s `process` feature is enabled for signal delivery.

## R3 / F3 — P1: live-session restoration failure dropped exclusivity

- **Fix:** `SessionInner::restore` now returns its outcome instead of silently
  swallowing a failure. `OwnToneSession::close` keeps the advisory lock and the
  route and installs the same serialized recovery on a failed restoration; the
  route is revoked by identity only at the terminal disposition.
  `natural_completion` and the pump's error path publish `Error` + `Stopped`
  instead of `TrackEnded` when restoration fails, so a half-taken-over daemon is
  never reported as clean completion.
- **Tests:** `restore_reports_a_failed_restoration`,
  `failed_restoration_preserves_the_takeover_record`.

## R4 / F2 — P1: OwnTone started the pump before stale-open acceptance

- **Fix:** the decode pump stays inert behind a deterministic activation gate
  (`activation_gate` / `wait_for_activation`). It starts only after
  `OwnToneSession::resume` accepts a current-generation activation — the load
  worker calls `resume` only after confirming the generation is current and the
  load was not cancelled — and it aborts the wait the moment the load is
  cancelled or torn down. A cancelled or replaced load therefore starts no
  pipeline, writes no PCM, publishes no start event and drives no daemon play.
- **Test:** `activation_gate_releases_only_on_accepted_activation` covers
  release-on-acceptance plus the cancelled, torn-down and never-activated inert
  cases.

## R5 / F5 — P1: ownership record did not verify the answering process

- **Fix:** `verify_owned` is now the record check **plus**
  `verify_daemon_process`, which resolves the process bound to the configured
  loopback API port out of band through `/proc/net/tcp{,6}` (LISTEN inode) and
  `/proc/<pid>/fd` (socket-inode owner), then requires the listener's executable
  to be the configured binary and its command line to bind the configured state
  directory. A stale matching record can no longer authorize a foreign
  instance that reuses the port. The record-only check stays in `probe` so the
  `/proc` walk does not run on GTK.
- **Tests:** `listening_inodes_parses_the_proc_net_tcp_table`,
  `listener_process_resolves_the_process_bound_to_a_port`,
  `cmdline_binding_requires_the_exact_state_directory`,
  `verify_daemon_process_refuses_a_foreign_listener`.

## Prior finding disposition (F1–F5)

- **F1** — unchanged: the Unix module gating plus the not-Unix shim still
  address the unconditional-import defect; no Windows build performed here.
- **F2** — R1 and R4 close the residual worker-conversion gaps; production
  controller-path coverage is now present (R1 regression + R4 gate test).
- **F3** — R2 and R3 close the residual record-retention and recovery-ownership
  gaps; quiescence is now executable, not a retained-file promise.
- **F4** — the pause-vs-stop predicate is now regression-tested
  (`completion_requires_stop_and_never_accepts_pause`); the code still closes the
  owned FIFO write end before waiting for daemon confirmation and accepts only
  `stop`, which is a code-level guarantee (a live FIFO/daemon EOF test is out of
  this adapter's unit scope).
- **F5** — R5 closes the endpoint-reuse gap.

## Gates at this head

- `cargo check --all-targets --locked`
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --release -- -D warnings`
- `cargo build --release`
- `cargo test --all-targets` (1907 lib + 30 packaging, 0 failed)
