# tr-d3z — Watcher backlog / root-confirmation ordering harness JUDGEMENT

**Bead:** tr-d3z (P3.4 backlog)
**Source record:** `docs/task.md`, P3.4 — Maintenance and coverage
**Decision date:** 2026-07-25
**Revised:** 2026-09-03 after the exact-head review of PR #176. The verdict
(DO NOT build the platform-fixture harness) is unchanged, but the review
showed the original evidence did not support it: two deterministic coverage
gaps were real, and three supporting claims about `notify` were wrong. This
revision corrects the record and recuts the deliverable around focused
deterministic tests that close those gaps.
**Decision:** DO NOT build the end-to-end live-backend harness. The two
deterministic gaps the exact-head review identified — the untested
pending-root-trust `process_directory_events` boundary and the masked
standalone `RenameMode::Any` coverage — are closed by focused unit tests
instead. The live-backend gaps (real callback chain, real debounce timer)
remain explicit, accepted gaps.

## 1. What the record asks for

`docs/task.md`, under P3.4, asks:

> Add a direct end-to-end watcher-backlog/root-confirmation ordering harness
> **if its incremental coverage remains worth the platform-fixture cost**.

The current bead description makes the conditionality explicit:

> First deliverable is therefore a judgement: inventory the ordering invariants
> already covered, identify the live-backend gap, and decide whether that gap
> justifies the fixture. Build the harness only if the answer is yes.

This document records the judgement, the corrected supporting analysis, and
the boundary of what the decision does and does not decide.

## 2. Coverage audit — what already exists

The deterministic ordering properties are pinned by focused tests inside
`src/local/engine.rs`. The relevant group:

### 2.1 Watcher-backlog discard ordering

| Test | Property pinned |
|------|-----------------|
| `watcher_ingress_filters_access_noise_before_the_bounded_queue` | `Access` events and atime-only `Modify(Metadata(AccessTime))` events never enter the bounded queue; real mutations after the storm remain queueable; backend `Rescan` survives filtering; backend errors survive filtering. Other `Metadata` kinds (write, permissions, ownership) are **not** filtered — they are potentially mutating and stay fail-closed in the queue. |
| `watcher_ingress_overflow_is_nonblocking_and_marks_stream_unreliable` | `try_send` `Full` does not block; `ingress_overflowed` flag is authoritative; the event admitted before overflow remains queued. |
| `watcher_error_and_rescan_notice_make_debounce_unreliable` | Backend error and `Rescan` flag both make `WatcherDebounceBatch::finish` return `None`, forcing reconciliation. |
| `watcher_error_discards_mixed_incremental_batch_and_backlog` | Mixed incremental + `notify::Error` + stale queued event: the incremental batch is discarded, the queue is drained, no event is applied. |
| `watcher_reconciliation_preserves_racing_overflow_and_new_events` | After a stale backlog is drained, an event arriving during the reconciliation scan remains queued for the next loop iteration; the overflow signal is not cleared at the end of the scan. |
| `pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events` | The pending-root-trust boundary in `process_directory_events` (the `pending_trust_scan.take()` branch): watcher evidence queued before the distinct ordinary authority scan is suppressed **even though the stream is healthy** (no error, no overflow); the suppressed evidence is demonstrably actionable (an identical event yields a non-empty incremental upsert batch); a track whose evidence was suppressed is still delivered — by the authority scan, not the incremental; evidence racing the authority scan remains queued for the next boundary; the real completion runs (`finish_pending_root_trust_scan` records an `Active` outcome); the overflow signal is neither consulted nor cleared. |
| `watcher_retries_a_root_that_appears_during_bootstrap` | `install_directory_watcher` + `watch_available_directories` retain old registrations and close new gaps when a missing root appears mid-bootstrap. |
| `discard_watcher_backlog` (helper, invoked at two production sites and in two unit tests) | Drain of `mpsc::Receiver<notify::Result<notify::Event>>` is best-effort and total; events arriving after the drain remain queued. |

The four invocations of `discard_watcher_backlog` are:

1. Production: `reconcile_unreliable_watcher_stream`
   (src/local/engine.rs:5336) — drain
   on stream-loss before the authoritative scan.
2. Production: `process_directory_events` (src/local/engine.rs:5393) — drain at a
   pending-root-trust-scan boundary.
3. Unit test: `watcher_error_discards_mixed_incremental_batch_and_backlog`.
4. Unit test: `watcher_reconciliation_preserves_racing_overflow_and_new_events`.

Together these cover: **when the backlog is discarded**, **what survives the
discard**, and **what survives the subsequent authoritative scan** — now
including the pending-root-trust boundary itself.

### 2.2 Root-confirmation ordering

The root-trust slice is exercised through `RootTrustReason` decisions,
`build_root_trust_request`, and `root_trust_request_id`:

| Test | Property pinned |
|------|-----------------|
| `root_trust_reasons_cover_legacy_replacement_and_empty_evidence` | `LegacyEnrollment`, `EmptyRoot`, `Replacement` reasons each map to the right scan evidence; `EmptyRoot` request requires acknowledgement. |
| `root_trust_requires_complete_exact_configured_evidence` | A nested-discovered root cannot be confirmed; an incomplete traversal cannot be confirmed. |
| `root_trust_request_id_ignores_timestamps_but_binds_security_state` | Two scans of the same path with only `last_checked_at` differ produce identical `request_id`; changing `is_available` changes `request_id`. |
| `root_trust_request_debug_redacts_private_evidence` | `Debug` redact excludes secret-bearing evidence. |
| `forced_trust_conversion_preserves_all_tracks_until_ordinary_scan` | The conversion scan writes no track rows; the distinct ordinary authority scan delivered by `complete_root_trust_scan` is what confirms identity and applies content. |

The ordering between root-trust evidence and watcher events is pinned at the
boundary by `pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events`
(§2.1). The reason a root-confirmation request is built and the form of the
request are both unit-covered.

### 2.3 Debounce → batch ordering

| Test | Property pinned |
|------|-----------------|
| `watcher_batch_normalizes_both_rename_without_fallback_paths` | `RenameMode::Both` produces exactly one `rename_pair`; no fallback `remove`+`upsert`. |
| `watcher_batch_deduplicates_linux_from_to_and_both_events` | `From`+`To`+`Both` with matching tracker collapses to a single `rename_pair`. |
| `watcher_batch_pairs_only_adjacent_untracked_windows_halves` | A non-adjacent pair promotes to `remove_paths` + `upsert_paths`. |
| `watcher_batch_name_any_alone_demands_reconciliation_without_identity` | A **standalone** `RenameMode::Any` event — no folder removal or other event that could mask the routing — forces `reconciliation_required`, and alone produces no rename pair, no upsert, no remove, no deferred path, no dirty directory scope: identity is never inferred from an unpaired `Name::Any`. |
| `watcher_batch_routes_unpairable_and_directory_events_to_reconciliation` | `RenameMode::Any` combined with `RemoveKind::Folder` forces reconciliation; the folder path remains deferred as a dirty scope for a paired parent rename. Note this combined test alone could not attribute `reconciliation_required` to the `Name::Any` half — the standalone test above removes that ambiguity. |
| `watcher_batch_queues_regular_and_missing_audio_paths_only` | Both a present file and a vanished file reach `upsert_paths`; only a vanished file is held for the guarded removal backstop. |
| `watcher_batch_defers_directory_rename_halves_until_the_pair_is_known` | A directory `From` alone is not promoted to reconciliation; the `To` completes the pair without rescan. |
| `watcher_batch_promotes_an_unclaimed_directory_removal_to_reconciliation` | A directory `From` without a paired `To` forces reconciliation. |
| `watcher_batch_rejects_rename_pairs_nested_in_a_renamed_directory` | A nested pair inside a renamed directory forces reconciliation (tracker ordering cannot decide). |
| `marker_mutation_requires_reconciliation_before_incrementals` | A batch containing both a marker file and a regular upsert requires reconciliation before any incremental is applied; the marker root is recorded in `identity_changed_roots`. |
| `watcher_ignores_tag_siblings_and_refreshes_the_replaced_track` | A private `.tributary-tag-*.flac` sibling does not produce a real change; the public track is refreshed. |

These tests pin the ordering properties of `WatcherBatch::collect`,
`WatcherBatch::finish`, `WatcherDebounceBatch::finish`, and the
`requires_reconciliation_before_incrementals` invariant.

## 3. What an end-to-end harness would still add

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

3. **Cross-platform backend event emission.** In pinned `notify` 8.2.0, the
   *handling* of every backend rename shape is unit-covered (§2.3): the
   Linux/inotify tracked `From`/`To`/`Both` shape, the FSEvents-style
   `Modify(Name::Any)` shape (now standalone), and the Windows untracked
   `From`/`To` shape. What remains uncovered is which shape each backend
   **actually emits** for a given filesystem mutation — a live-backend
   observation no synthetic event can substitute for.

## 4. Fixture cost analysis

Building an end-to-end harness that exercises the live `notify` backend
requires:

- **Compile-time backend selection.** `notify::RecommendedWatcher` resolves to
  a concrete backend (`INotifyWatcher`, `FsEventWatcher`,
  `ReadDirectoryChangesWatcher`) via `#[cfg]` at **compile time**, not
  runtime. There is no runtime backend pin to configure: each platform's CI
  job compiles and would run exactly its native backend. The coverage
  aggregate and enforced floor run only on Linux x86_64, although CI also
  runs tests on macOS and Windows. The harness's marginal cost is therefore
  not a per-platform fixture *design* — it is that any timing-sensitive
  assertion must be stable on three differently loaded CI runners, and
  coverage credit accrues only from the Linux run.

- **Timing-sensitive async loop.** The harness needs `tokio::test` with a
  `process_directory_events`-like driver to apply real filesystem mutations
  and observe resulting batches. The driver must tolerate backend coalescing
  delay (not deterministic) and must not race the debounce window. Real
  harness runs therefore need either `#[tokio::test(start_paused = true)]`
  with manual time advance or generous `sleep` budgets.

- **Filesystem fixture lifetime.** `TestDirectory` (Drop deletes the
  directory) is the existing pattern. The notify callback receives events
  through an owned channel closure and never borrows the fixture, so the
  test body can own the `TestDirectory` directly; no `Arc<TestDirectory>`
  sharing boundary is required. The lifetime constraint is only that the
  directory must outlive the watch registration, which the existing drop
  guard already satisfies.

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

## 5. Decision boundary

This decision decides:

- Whether to build the direct end-to-end watcher-backlog/root-confirmation
  ordering harness described in `docs/task.md` P3.4. **No** — for the
  platform-fixture and timing cost documented in §4.
- How to close the two deterministic gaps the exact-head review of PR #176
  identified. **With focused unit tests** (`pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events`,
  `watcher_batch_name_any_alone_demands_reconciliation_without_identity`),
  not with the harness.

This decision does **not** decide:

- The accepted live-backend gaps (§3.1, §3.2): real `notify` callback firing
  order and real debounce-window expiry. They remain uncovered by design.
- Any Windows- or macOS-specific watcher fix. If a backend-specific bug
  forces one, that is a new, conditional decision (§7).
- The coverage floor. `coverage-baseline.txt` (66.9) is unchanged by this
  bead, per the 2026-07-17 P3.5 decision.

## 6. Net judgement

The properties pinned by the focused tests in §2 are the **deterministic
behavioral** properties the end-to-end harness would re-prove. The exact-head
review of PR #176 found the original evidence for "no harness needed"
unsupported on two counts — no test exercised the pending-root-trust
`process_directory_events` boundary, and the sole `RenameMode::Any` test was
masked by a subsequent folder removal — and flagged three incorrect claims
(runtime backend selection, `Arc<TestDirectory>` fixture requirements, and an
over-broad metadata-filter description). This revision corrects all three
(§2.1, §4) and closes both deterministic gaps with focused tests (§8). The
recut was made around exactly those tests, as the review directed.

The remaining uncovered properties (§3.1, §3.2 — live callback chain, real
1500ms debounce-window expiry) have different tradeoffs:

- Existing `enqueue_watcher_result` tests cover the callback → queue edge at a
  lower layer, but the live end-to-end callback chain and real debounce-window
  expiry remain untested.
- Their assertions must be stable across three differently loaded CI runners
  while coverage credit accrues only on Linux x86_64.
- Subject to host-load variance in CI (real debounce-window timing).

The fixture cost (§4) therefore remains high relative to the marginal coverage
gain: the harness would add timing-sensitive, host-load-dependent evidence for
gaps that are explicit and accepted, while the deterministic properties it
would re-prove are now pinned without it.

**Verdict:** the harness is not justified at this time. The focused unit
coverage — including the two tests this bead adds — protects the deterministic
ordering invariants, while live callback ordering and real debounce-window
expiry remain explicit, accepted gaps. The platform-fixture and timing cost
does not justify closing those gaps without incident evidence that makes them
load-bearing.

## 7. Forward-looking note

If the gap in §3.3 (cross-platform backend emission) becomes load-bearing —
e.g., a Windows-specific bug report forces a Windows-only fix — a
Windows-only `#[cfg(windows)]` integration test would be the right shape, not
the harness described here. That is a future, conditional decision and out of
scope for this bead.

## 8. Standalone verification

Every coverage claim in this record is checkable from the repository root
without any platform fixture. Each named test exists exactly once in
`src/local/engine.rs` (verified verbatim against the head of the branch that
carries this revision).

The two tests this bead adds:

```sh
cargo test --bin tributary local::engine::tests::pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events
cargo test --bin tributary local::engine::tests::watcher_batch_name_any_alone_demands_reconciliation_without_identity
```

The pre-existing tests cited in §2, runnable as one group:

```sh
cargo test --bin tributary local::engine::tests::watcher
cargo test --bin tributary local::engine::tests::forced_trust_conversion_preserves_all_tracks_until_ordinary_scan
cargo test --bin tributary local::engine::tests::root_trust
cargo test --bin tributary local::engine::tests::marker_mutation_requires_reconciliation_before_incrementals
cargo test --bin tributary local::engine::tests::watcher_batch
```

Name-existence check for every test named in this document:

```sh
grep -c "fn watcher_batch_name_any_alone_demands_reconciliation_without_identity(" src/local/engine.rs
grep -c "fn pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events(" src/local/engine.rs
```

(Each prints `1`; the remaining §2 names were verified the same way against
this branch's head.)

## 9. Acceptance

This document records the revised JUDGEMENT requested by `tr-d3z`, and the
recut deliverable: two focused deterministic tests in `src/local/engine.rs`
closing the boundary and `Name::Any` gaps. No harness is built.
`coverage-baseline.txt` is unchanged. The bead should be reassigned to
refinery so the judgement enters the ledger and any subsequent P3.4 review
can cite it.
