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
**Revised:** 2026-09-05 after the semantic review of PR #222. Main meanwhile
landed the loop-level end-to-end ordering harness (tr-yu10n,
`marker_mutation_confirms_root_before_backlog_incrementals_end_to_end`), so
the P3.4 conditional — add a direct end-to-end harness *if* its incremental
coverage remains worth the platform-fixture cost — is already answered for
the deterministic ordering contract. This revision rescopes the judgement to
what remains undecided (the live-backend platform-fixture harness) and recuts
this bead's boundary test to drive the real `process_directory_events` loop
with mid-scan injection.

**Decision:** the direct end-to-end watcher-backlog/root-confirmation
ordering harness that P3.4 conditionally asked for **exists on main** (the
tr-yu10n loop-level harness, §2.4). This bead does not duplicate it; its own
deliverable is the loop-driven pending-root-trust boundary test (§2.1).
DO NOT build the remaining live-backend platform-fixture harness: its only
incremental coverage — real `notify` callback ordering, real debounce-timer
expiry, and per-platform backend emission shapes — is explicitly accepted as
uncovered (§3), and the fixture and timing cost (§4) does not justify it
without incident evidence that makes those gaps load-bearing.

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

The ordering properties are pinned at three layers inside
`src/local/engine.rs`: focused deterministic unit tests (§2.1–§2.3), this
bead's loop-level boundary test (§2.1), and the loop-level end-to-end
harness main already carries (§2.4).

### 2.1 Watcher-backlog discard ordering

- `watcher_ingress_filters_access_noise_before_the_bounded_queue` — `Access`
  events and atime-only `Modify(Metadata(AccessTime))` events never enter the
  bounded queue; real mutations after the storm remain queueable; backend
  `Rescan` survives filtering; backend errors survive filtering. Other
  `Metadata` kinds (write, permissions, ownership) are **not** filtered — they
  are potentially mutating and stay fail-closed in the queue.
- `watcher_ingress_overflow_is_nonblocking_and_marks_stream_unreliable` —
  `try_send` `Full` does not block; `ingress_overflowed` flag is
  authoritative; the event admitted before overflow remains queued.
- `watcher_error_and_rescan_notice_make_debounce_unreliable` — backend error
  and `Rescan` flag both make `WatcherDebounceBatch::finish` return `None`,
  forcing reconciliation.
- `watcher_error_discards_mixed_incremental_batch_and_backlog` — mixed
  incremental + `notify::Error` + stale queued event: the incremental batch
  is discarded, the queue is drained, no event is applied.
- `watcher_reconciliation_preserves_racing_overflow_and_new_events` — after a
  stale backlog is drained, an event arriving during the reconciliation scan
  remains queued for the next loop iteration; the overflow signal is not
  cleared at the end of the scan.
- `pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events` —
  this bead's boundary test, recut 2026-09-05 to drive the **real**
  `process_directory_events` loop: a `ConfirmRootTrust` command queues the
  pending trust scan inside the loop, and the loop's own
  `pending_trust_scan.take()` boundary performs the backlog discard and the
  distinct ordinary authority scan. Watcher evidence queued before the
  boundary is suppressed **even though the stream is healthy** (no error, no
  overflow); the suppressed evidence is demonstrably actionable (an identical
  event yields a non-empty incremental upsert batch); a track whose evidence
  was suppressed is still delivered — by the authority scan's `FullSync`, not
  by an incremental. The racing event is injected **only after the authority
  scan has begun**, deterministically synchronized on the scan's per-file
  `ScanProgress` event (consumed only after the conversion scan's
  `ScanComplete`), so it is distinguishable from backlog that escaped the
  discard: it survives the boundary and applies strictly after the scan
  boundary. The pending command completes with outcome `Active`, and the
  overflow signal is never set.
- `watcher_retries_a_root_that_appears_during_bootstrap` —
  `install_directory_watcher` + `watch_available_directories` retain old
  registrations and close new gaps when a missing root appears mid-bootstrap.
- `discard_watcher_backlog` (helper, invoked at two production sites and in
  two unit tests) — drain of
  `mpsc::Receiver<notify::Result<notify::Event>>` is best-effort and total;
  events arriving after the drain remain queued.

The four invocations of `discard_watcher_backlog` are:

1. Production: `reconcile_unreliable_watcher_stream`
   (src/local/engine.rs:5336) — drain
   on stream-loss before the authoritative scan.
2. Production: `process_directory_events` (src/local/engine.rs:5393) — drain at a
   pending-root-trust-scan boundary. This is the exact boundary the recut
   test above now drives through the real loop.
3. Unit test: `watcher_error_discards_mixed_incremental_batch_and_backlog`.
4. Unit test: `watcher_reconciliation_preserves_racing_overflow_and_new_events`.

Together these cover: **when the backlog is discarded**, **what survives the
discard**, and **what survives the subsequent authoritative scan** — now
including the pending-root-trust boundary itself, exercised end-to-end
through `process_directory_events`.

### 2.2 Root-confirmation ordering

The root-trust slice is exercised through `RootTrustReason` decisions,
`build_root_trust_request`, and `root_trust_request_id`:

- `root_trust_reasons_cover_legacy_replacement_and_empty_evidence` —
  `LegacyEnrollment`, `EmptyRoot`, `Replacement` reasons each map to the
  right scan evidence; `EmptyRoot` request requires acknowledgement.
- `root_trust_requires_complete_exact_configured_evidence` — a
  nested-discovered root cannot be confirmed; an incomplete traversal cannot
  be confirmed.
- `root_trust_request_id_ignores_timestamps_but_binds_security_state` — two
  scans of the same path with only `last_checked_at` differ produce identical
  `request_id`; changing `is_available` changes `request_id`.
- `root_trust_request_debug_redacts_private_evidence` — `Debug` redact
  excludes secret-bearing evidence.
- `forced_trust_conversion_preserves_all_tracks_until_ordinary_scan` — the
  conversion scan writes no track rows; the distinct ordinary authority scan
  delivered by `complete_root_trust_scan` is what confirms identity and
  applies content.

The ordering between root-trust evidence and watcher events is pinned at the
boundary by `pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events`
(§2.1), now driving the real loop, and at the marker-mutation layer by the
landed harness (§2.4). The reason a root-confirmation request is built and
the form of the request are both unit-covered.

### 2.3 Debounce → batch ordering

- `watcher_batch_normalizes_both_rename_without_fallback_paths` —
  `RenameMode::Both` produces exactly one `rename_pair`; no fallback
  `remove`+`upsert`.
- `watcher_batch_deduplicates_linux_from_to_and_both_events` — `From`+`To`+
  `Both` with matching tracker collapses to a single `rename_pair`.
- `watcher_batch_pairs_only_adjacent_untracked_windows_halves` — a
  non-adjacent pair promotes to `remove_paths` + `upsert_paths`.
- `watcher_batch_name_any_alone_demands_reconciliation_without_identity` — a
  **standalone** `RenameMode::Any` event — no folder removal or other event
  that could mask the routing — forces `reconciliation_required`, and alone
  produces no rename pair, no upsert, no remove, no deferred path, no dirty
  directory scope: identity is never inferred from an unpaired `Name::Any`.
- `watcher_batch_routes_unpairable_and_directory_events_to_reconciliation` —
  `RenameMode::Any` combined with `RemoveKind::Folder` forces reconciliation;
  the folder path remains deferred as a dirty scope for a paired parent
  rename. Note this combined test alone could not attribute
  `reconciliation_required` to the `Name::Any` half — the standalone test
  above removes that ambiguity.
- `watcher_batch_queues_regular_and_missing_audio_paths_only` — both a
  present file and a vanished file reach `upsert_paths`; only a vanished file
  is held for the guarded removal backstop.
- `watcher_batch_defers_directory_rename_halves_until_the_pair_is_known` — a
  directory `From` alone is not promoted to reconciliation; the `To` completes
  the pair without rescan.
- `watcher_batch_promotes_an_unclaimed_directory_removal_to_reconciliation` —
  a directory `From` without a paired `To` forces reconciliation.
- `watcher_batch_rejects_rename_pairs_nested_in_a_renamed_directory` — a
  nested pair inside a renamed directory forces reconciliation (tracker
  ordering cannot decide).
- `marker_mutation_requires_reconciliation_before_incrementals` — a batch
  containing both a marker file and a regular upsert requires reconciliation
  before any incremental is applied; the marker root is recorded in
  `identity_changed_roots`.
- `watcher_ignores_tag_siblings_and_refreshes_the_replaced_track` — a private
  `.tributary-tag-*.flac` sibling does not produce a real change; the public
  track is refreshed.

These tests pin the ordering properties of `WatcherBatch::collect`,
`WatcherBatch::finish`, `WatcherDebounceBatch::finish`, and the
`requires_reconciliation_before_incrementals` invariant.

### 2.4 The landed loop-level end-to-end harness (tr-yu10n)

`marker_mutation_confirms_root_before_backlog_incrementals_end_to_end`
(src/local/engine.rs) is the end-to-end watcher-backlog/root-confirmation
ordering harness that P3.4 conditionally asked for. It was landed on main by
tr-yu10n and is part of the base this bead's branch carries. Its doc comment
self-describes the shape: it "Drives the real `process_directory_events`
loop with a synthetic event channel: a genuine `RecommendedWatcher` backend
with zero installed watches contributes no platform event timing, so the
ordering contract is exercised deterministically without the cost of a live
inotify/FSEvents/ReadDirectoryChangesW fixture."

It pins: a marker mutation mixed with an incremental upsert in one debounced
batch must be consumed entirely by root confirmation; no per-track
incremental event may precede the confirmation scan's `ScanComplete`; a
further event queued behind the batch (the watcher backlog) applies only
after the root is re-confirmed.

This bead does not duplicate that harness. Its boundary test (§2.1)
complements it: the landed harness pins marker-mutation ordering, while the
boundary test pins the pending-root-trust `pending_trust_scan.take()`
boundary — backlog suppression and racing-evidence retention around the
distinct ordinary authority scan, with the racing event injected mid-scan via
deterministic synchronization.

## 3. What remains uncovered: the live-backend observation layer

The loop-level harness (§2.4) and the focused tests above cover the
deterministic ordering contract. What remains uncovered is precisely what
only the live `notify` backend can observe:

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

- Whether this bead should add a further end-to-end ordering harness of its
  own. **No** — the loop-level end-to-end harness already exists on main
  (§2.4), and this bead's boundary test (§2.1) closes the remaining
  deterministic boundary gap; a duplicate would add no coverage.
- Whether to build the live-backend platform-fixture harness (§3). **No** —
  for the platform-fixture and timing cost documented in §4.
- How to close the two deterministic gaps the exact-head review of PR #176
  identified. **With focused deterministic tests** — the pending-root-trust
  boundary is closed by
  `pending_root_trust_boundary_suppresses_backlog_and_keeps_racing_events`,
  recut to drive the real `process_directory_events` loop with mid-scan
  injection; the masked standalone `RenameMode::Any` coverage is closed by
  `watcher_batch_name_any_alone_demands_reconciliation_without_identity`.

This decision does **not** decide:

- The accepted live-backend gaps (§3.1, §3.2): real `notify` callback firing
  order and real debounce-window expiry. They remain uncovered by design.
- Any Windows- or macOS-specific watcher fix. If a backend-specific bug
  forces one, that is a new, conditional decision (§7).
- The coverage floor. `coverage-baseline.txt` (66.9) is unchanged by this
  bead, per the 2026-07-17 P3.5 decision.

## 6. Net judgement

The deterministic ordering contract the P3.4 harness question is about is
now pinned at three layers: the focused unit tests (§2.1–§2.3), this bead's
loop-level boundary test (§2.1), and the loop-level end-to-end harness main
already carries (§2.4). The 2026-09-03 revision of this record closed the two
deterministic gaps the PR #176 review found. The 2026-09-05 recut, made after
the semantic review of PR #222 rejected the previous head for contradicting
current main, drives that boundary test through the real
`process_directory_events` loop and injects the racing event mid-scan via
deterministic synchronization — exactly as that review directed — and
rescopes this record to cite the landed tr-yu10n harness instead of
claiming no harness exists.

The remaining uncovered properties (§3 — live callback chain, real 1500ms
debounce-window expiry, per-platform backend emission shapes) have different
tradeoffs:

- Existing `enqueue_watcher_result` tests cover the callback → queue edge at a
  lower layer, but the live end-to-end callback chain and real debounce-window
  expiry remain untested.
- Their assertions must be stable across three differently loaded CI runners
  while coverage credit accrues only on Linux x86_64.
- Subject to host-load variance in CI (real debounce-window timing).

The fixture cost (§4) therefore remains high relative to the marginal
coverage gain: a live-backend harness would add timing-sensitive,
host-load-dependent evidence for gaps that are explicit and accepted, while
the deterministic properties it would re-prove are already pinned without
it — including, since tr-yu10n, the end-to-end ordering itself.

**Verdict:** the harness question P3.4 asked is settled for the
deterministic ordering contract — that harness exists on main, and this
bead's boundary test complements it. The live-backend platform-fixture
harness is not justified at this time: live callback ordering, real
debounce-window expiry, and backend emission shapes remain explicit,
accepted gaps until incident evidence makes them load-bearing.

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

The landed tr-yu10n harness cited in §2.4, runnable as:

```sh
cargo test --bin tributary local::engine::tests::marker_mutation_confirms_root_before_backlog_incrementals_end_to_end
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
grep -c "fn marker_mutation_confirms_root_before_backlog_incrementals_end_to_end(" src/local/engine.rs
```

(Each prints `1`; the remaining §2 names were verified the same way against
this branch's head.)

## 9. Acceptance

This document records the 2026-09-05 rescope of the tr-d3z JUDGEMENT to
current reality: the end-to-end watcher-backlog/root-confirmation ordering
harness P3.4 conditionally requested exists on main (tr-yu10n, §2.4); this
bead's deliverable is the loop-driven pending-root-trust boundary test
(§2.1) plus this record. The live-backend platform-fixture harness is not
built — verdict unchanged from the 2026-09-03 revision — and its gaps remain
explicit and accepted. `coverage-baseline.txt` is unchanged. The bead should
be reassigned to refinery so the judgement enters the ledger and any
subsequent P3.4 review can cite it.
