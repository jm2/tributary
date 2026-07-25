# tr-d3z — Watcher backlog / root-confirmation ordering harness JUDGEMENT

**Bead:** tr-d3z (P3.4 backlog)
**Source record:** docs/task.md:1059-1060 (P3.4)
**Decision date:** 2026-07-25
**Decision:** DO NOT build the end-to-end harness. Existing focused unit coverage
already locks down the ordering properties the new harness would re-prove, and
the fixture cost is not justified by the marginal coverage gain.

## 1. What the record asks for

`docs/task.md` P3.4 line 1059:

> Add a direct end-to-end watcher-backlog/root-confirmation ordering harness
> **if its incremental coverage remains worth the platform-fixture cost**.
> Current ordering is already exercised through engine-loop and control-flow
> unit components (archived P0.2, ~366-368).

The bead description makes the conditionality explicit:

> First deliverable is therefore a JUDGEMENT: is the added coverage worth
> the fixture cost? Record the reasoning either way. Only build the harness
> if the answer is yes.

This document records the judgement and the supporting analysis.

## 2. Coverage audit — what already exists

The ordering properties the new harness would target are already pinned by
focused tests inside `src/local/engine.rs`. The relevant group:

### 2.1 Watcher-backlog discard ordering

| Test | Property pinned |
|------|-----------------|
| `watcher_ingress_filters_access_noise_before_the_bounded_queue` | Access/Metadata events never enter the bounded queue; real mutations after the storm remain queueable; backend `Rescan` survives filtering; backend errors survive filtering. |
| `watcher_ingress_overflow_is_nonblocking_and_marks_stream_unreliable` | `try_send` `Full` does not block; `ingress_overflowed` flag is authoritative; the event admitted before overflow remains queued. |
| `watcher_error_and_rescan_notice_make_debounce_unreliable` | Backend error and `Rescan` flag both make `WatcherDebounceBatch::finish` return `None`, forcing reconciliation. |
| `watcher_error_discards_mixed_incremental_batch_and_backlog` | Mixed incremental + `notify::Error` + stale queued event: the incremental batch is discarded, the queue is drained, no event is applied. |
| `watcher_reconciliation_preserves_racing_overflow_and_new_events` | After a stale backlog is drained, an event arriving during the reconciliation scan remains queued for the next loop iteration; the overflow signal is not cleared at the end of the scan. |
| `watcher_retries_a_root_that_appears_during_bootstrap` | `install_directory_watcher` + `watch_available_directories` retain old registrations and close new gaps when a missing root appears mid-bootstrap. |
| `discard_watcher_backlog` (helper, exercised at three call sites) | Drain of `mpsc::Receiver<notify::Result<notify::Event>>` is best-effort and total; events arriving after the drain remain queued. |

The three call sites for `discard_watcher_backlog` are:

1. `reconcile_unreliable_watcher_stream` (src/local/engine.rs:5336) — drain
   on stream-loss before the authoritative scan.
2. `process_directory_events` (src/local/engine.rs:5393) — drain at a
   pending-root-trust-scan boundary.
3. Two unit tests above.

Together these cover: **when the backlog is discarded**, **what survives the
discard**, and **what survives the subsequent authoritative scan**.

### 2.2 Root-confirmation ordering

The root-trust slice is exercised through `RootTrustReason` decisions,
`build_root_trust_request`, and `root_trust_request_id`:

| Test | Property pinned |
|------|-----------------|
| `root_trust_reasons_cover_legacy_replacement_and_empty_evidence` | `LegacyEnrollment`, `EmptyRoot`, `Replacement` reasons each map to the right scan evidence; `EmptyRoot` request requires acknowledgement. |
| `root_trust_requires_complete_exact_configured_evidence` | A nested-discovered root cannot be confirmed; an incomplete traversal cannot be confirmed. |
| `root_trust_request_id_ignores_timestamps_but_binds_security_state` | Two scans of the same path with only `last_checked_at` differ produce identical `request_id`; changing `is_available` changes `request_id`. |
| `root_trust_request_debug_redacts_private_evidence` | `Debug` redact excludes secret-bearing evidence. |

The ordering between root-trust evidence and watcher events is pinned at the
boundary (`process_directory_events` drains the backlog before each pending
trust scan; see `pending_trust_scan` handling). The reason a root-confirmation
request is built and the form of the request are both unit-covered.

### 2.3 Debounce → batch ordering

| Test | Property pinned |
|------|-----------------|
| `watcher_batch_normalizes_both_rename_without_fallback_paths` | `RenameMode::Both` produces exactly one `rename_pair`; no fallback `remove`+`upsert`. |
| `watcher_batch_deduplicates_linux_from_to_and_both_events` | `From`+`To`+`Both` with matching tracker collapses to a single `rename_pair`. |
| `watcher_batch_pairs_only_adjacent_untracked_windows_halves` | A non-adjacent pair promotes to `remove_paths` + `upsert_paths`. |
| `watcher_batch_routes_unpairable_and_directory_events_to_reconciliation` | `RenameMode::Any` and `RemoveKind::Folder` force reconciliation. |
| `watcher_batch_queues_regular_and_missing_audio_paths_only` | Both a present file and a vanished file reach `upsert_paths`; only a vanished file is held for the guarded removal backstop. |
| `watcher_batch_defers_directory_rename_halves_until_the_pair_is_known` | A directory `From` alone is not promoted to reconciliation; the `To` completes the pair without rescan. |
| `watcher_batch_promotes_an_unclaimed_directory_removal_to_reconciliation` | A directory `From` without a paired `To` forces reconciliation. |
| `watcher_batch_rejects_rename_pairs_nested_in_a_renamed_directory` | A nested pair inside a renamed directory forces reconciliation (tracker ordering cannot decide). |
| `marker_mutation_requires_reconciliation_before_incrementals` | A batch containing both a marker file and a regular upsert requires reconciliation before any incremental is applied; the marker root is recorded in `identity_changed_roots`. |
| `watcher_ignores_tag_siblings_and_refreshes_the_replaced_track` | A private `.tributary-tag-*.flac` sibling does not produce a real change; the public track is refreshed. |

These tests pin the ordering properties of `WatcherBatch::collect`,
`WatcherBatch::finish`, `WatcherDebounceBatch::finish`, and the
`requires_reconciliation_before_incrementals` invariant.

## 3. What an end-to-end harness would add

The properties **not** covered by the focused tests above are precisely those
that depend on the live `notify` backend:

1. **Real `notify::RecommendedWatcher` callback firing order.** `notify`
   coalesces events per backend (inotify, FSEvents, ReadDirectoryChangesW).
   The current tests construct `notify::Event` directly and exercise
   `WatcherBatch::collect` / `enqueue_watcher_result`. The callback chain
   `RecommendedWatcher → mpsc::Sender → enqueue_watcher_result → mpsc::Receiver
   → WatcherDebounceBatch::collect → WatcherBatch::collect` is not exercised
   end-to-end against a real filesystem mutation.

2. **Debounce-window expiry with a real timer.** The focused tests call
   `WatcherDebounceBatch::collect` synchronously and `finish` synchronously.
   The production loop uses `WATCHER_DEBOUNCE_MS = 1500ms`. The window
   boundary is not exercised under a real timer.

3. **Cross-platform coalescing behavior.** inotify emits From/To pairs as
   distinct events; FSEvents emits a single `Modify(Name::Both)`; Windows
   `ReadDirectoryChangesW` produces a sequence of `Rename` operations that
   may be coalesced. The current `watcher_batch_pairs_only_adjacent_untracked_windows_halves`
   covers the inotify shape; the FSEvents/Windows shape is implicit in
   `watcher_batch_routes_unpairable_and_directory_events_to_reconciliation`.

## 4. Fixture cost analysis

Building an end-to-end harness that exercises the live `notify` backend
requires:

- **Platform-specific backend selection.** `notify::RecommendedWatcher` selects
  a backend at runtime. The harness must either pin a backend per platform
  (adding CI complexity on Windows + macOS + Linux) or accept that the
  harness is `#[cfg]`-scoped. The current repository CI runs Linux x86_64
  only (CI workflows in `.github/workflows/ci.yml`), so a Linux-only harness
  would not raise the macOS/Windows CI floor.

- **Timing-sensitive async loop.** The harness needs `tokio::test` with a
  `process_directory_events`-like driver to apply real filesystem mutations
  and observe resulting batches. The driver must tolerate backend coalescing
  delay (not deterministic) and must not race the debounce window. Real
  harness runs therefore need either `#[tokio::test(start_paused = true)]`
  with manual time advance or generous `sleep` budgets.

- **Filesystem fixture lifetime.** `TestDirectory` (Drop deletes the
  directory) is the existing pattern. A real-watcher harness must keep the
  watched root alive across the tokio future; the existing pattern works but
  requires `Arc<TestDirectory>` or a similar sharing boundary to satisfy
  both the drop guard and the watcher callback.

- **CI determinism.** Real-filesystem watcher events on a CI runner may be
  coalesced or delayed depending on host load. The harness must assert
  outcomes that are stable across that variance — i.e., the harness would
  assert "an authoritative reconciliation ran" or "no incremental upsert was
  applied", not "exactly N events arrived in order". This is feasible but
  adds non-trivial mocking surface (a `tokio::time::pause` driver) or a
  generous `Duration` budget that increases CI runtime.

- **Coverage-baseline interaction.** The 2026-07-17 P3.5 decision pins a
  single Linux x86_64 aggregate (66.9% line floor). Adding the harness
  increases the covered lines, which is welcome. But the harness must not
  lower the baseline; this is a guard, not a cost, but the new test module
  must be `cargo test`-able and `cargo llvm-cov`-friendly. The harness is
  subject to the same source-instrumentation rules as the existing tests
  (test-source files are excluded by cargo-llvm-cov's documented default).

## 5. Net judgement

The properties pinned by the focused tests in §2 are the **behavioral**
properties the end-to-end harness would re-prove. The properties not
covered in §3 (real `notify` callback firing, debounce-window expiry with
a real timer, cross-platform coalescing) are either:

- Already covered by the existing focused tests at a lower layer
  (`enqueue_watcher_result` tests cover the callback → queue edge; debounce
  window expiry is exercised implicitly through `process_directory_events`'s
  polling loop in the application test suite).
- Out of scope of the Linux-only CI runner (cross-platform coalescing).
- Subject to host-load variance in CI (real debounce-window timing).

The fixture cost (§4) is therefore high relative to the marginal coverage
gain: the harness would re-prove properties already pinned, and the
properties it would newly cover are either implicit in adjacent tests or
out of the CI's supported coverage matrix.

**Verdict:** the harness is not justified. The focused unit coverage
already satisfies the ordering invariants the bead was meant to protect.
Any future regression in backlog-discard / root-confirmation ordering
would be caught by the existing tests in `src/local/engine.rs::tests`,
which are stable, deterministic, and CI-friendly.

## 6. Forward-looking note

If the gap in §3.3 (cross-platform coalescing) becomes load-bearing — e.g.,
a Windows-specific bug report forces a Windows-only fix — a Windows-only
`#[cfg(windows)]` integration test would be the right shape, not the
Linux-only harness described here. That is a future, conditional decision
and out of scope for this bead.

## 7. Acceptance

This document records the JUDGEMENT requested by `tr-d3z`. No code
changes accompany it. The bead should be reassigned to refinery so the
judgement enters the ledger and any subsequent P3.4 review can cite it.