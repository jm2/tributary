# Engine / UI responsiveness measurement lane (Q4)

Scope: the **engine-loop and GTK-responsiveness** half of issue #275
(bead `tr-am6qr`). This lane measures, per runner:

- engine startup time-to-interactive components (server ready, scan
  full-sync publication, scan settlement),
- settlement of an in-flight scan, including a library command and the
  shutdown `Flush` barrier admitted **during** the scan,
- the GTK-side cost of publishing a scan result, which is
  simultaneously the **main-loop stall** the UI thread suffers,
  because the whole publication unit — Track→TrackObject conversion,
  playlist/queue refresh, the per-source clone, and the
  `display_local_tracks` redisplay including the folder-model rebuild —
  runs synchronously on the main loop.

The parse-side / large-library-import half (`tr-7nguk`) records its
own budgets in `docs/large-library-responsiveness.md`.

## Rule: budgets are agreed before optimizing

A budget miss gates optimization work — measure, agree a budget on the
affected runner, fix, re-measure — it never justifies silent widening.
Budgets below are recorded for the labelled runner; re-run the
harnesses on a different runner before trusting them elsewhere.

## Endpoints

### Engine side — `Q4_ENGINE_METRIC` lines

Harness:
`cargo test --bin tributary q4_engine_startup_and_during_scan_admission_benchmark -- --ignored --nocapture`
(`src/local/engine.rs` tests module). Size the fixture tree with
`TRIBUTARY_Q4_TRACKS` (default 400).

The test drives the production `LibraryEngine::run()` shape on a real
fixture directory with a trusted root row, and admits one
`RecordPlaybackHistory` command plus the shutdown `Flush{..}` barrier
**before** the engine task is first polled. `run()` awaits
`initial_scan` before its command loop, so both are structurally
during-scan admissions on any machine — no artificial parse delay
needed.

| Metric | Definition |
| --- | --- |
| `startup_server_ready_ms` | engine spawn → `ServerPlaylistRuntimeReady` |
| `startup_fullsync_ms` | engine spawn → `FullSync` publication of the initial scan |
| `startup_scan_settle_ms` | engine spawn → `ScanComplete` |
| `post_scan_drain_ms` | `ScanComplete` → command channel drained |
| `cancellation_flush_settle_ms` | engine spawn → `Flush` ack (scan settles first; pre-abort) |
| `scan_progress_events` | count of `ScanProgress` events (informational) |

Time-to-interactive ≈ `startup_fullsync_ms` (engine side) +
`fullsync_publication_first_ms` (GTK side below).

Settlement contract asserted by the harness (hard failure on breach):
`fullsync_rows == fixture rows + 1` (the pre-inserted command track is
part of the published snapshot), `ScanComplete` observed before the
`Flush` ack, and the command's `PlaybackHistoryUpdated` publication
observed only after `ScanComplete` — during-scan commands must not
publish before the scan settles.

### GTK side — `Q4_UI_METRIC` lines

Harness:
`q4_publication_contract_and_bench` (`src/ui/browser.rs` tests), called
from the crate's single consolidated GTK widget test
`gtk_widget_contracts_hold_on_one_session`. The contract half (60-row
`display_tracks` publication replaces the track store, master rows,
browser snapshot, and repopulates the genre pane — "All" + 4 genres)
runs in every display-backed test run; the measurement half runs only
when `TRIBUTARY_Q4_UI_BENCH_TRACKS=<rows>` is set:

```sh
# Headless: start a GTK 4 Broadway daemon, then run the widget test.
gtk4-broadwayd :97 &
GDK_BACKEND=broadway BROADWAY_DISPLAY=:97 DISPLAY=:97 \
  TRIBUTARY_Q4_UI_BENCH_TRACKS=10000 \
  cargo test --bin tributary gtk_widget_contracts -- --nocapture
```

| Metric | Definition |
| --- | --- |
| `fullsync_publication_first_ms` | the complete FullSync publication unit, 0 → N rows (see below) |
| `fullsync_publication_resync_ms` | the same complete unit, N → N rows (idempotent republication) |
| `publication_display_only_ms` | `display_tracks` alone, 0 → N rows (see below) |
| `browser_rebuild_ms` | `rebuild_browser_data` alone at N rows |

`fullsync_publication_first_ms` times the complete production
`LibraryEvent::FullSync` publication unit —
`window::apply_full_sync_publication`: arch Track→TrackObject
conversion, playlist/queue refresh, the per-source clone, and
`display_local_tracks` including the folder-model rebuild — 0 → N rows
on an empty browser. This is what the main loop blocks on at startup.
The measurement half calls the same function the production FullSync
arm calls (a source-structure test pins the arm to the unit).

`publication_display_only_ms` is a strict lower bound of
`fullsync_publication_first_ms`: `display_tracks` alone, 0 → N rows on
a second empty browser at the same scale, objects preconverted outside
the timer; the harness asserts the full path exceeds it at every scale.

The measurement half also sanity-asserts that the store, master rows,
browser snapshot, and per-source projection all carry exactly N rows
after publication, so a fast-but-wrong harness cannot pass.

## Recorded baselines and budgets

Budgets agreed 2026-09-18 pre-optimization. The measured values below
are conservative baselines for their labelled Linux debug-profile
runners: the harnesses are ignored measurement runs, not CI budget
gates, and optimized (release) builds are not measured here — they need
their own measurement run before any budget is claimed against them.

Engine numbers were **re-measured with the corrected arrival-time
sampler** (PR #291 review Correction 1: the first sampler revision
stamped each event before the `select!` wait, underreporting the
startup endpoints — most visibly `startup_server_ready_ms`, which had
been recorded as ~0.1 ms but is really ~2.3 ms on comparable runners).
The corrected engine table below is from runner **gastown.rictus dev
worktree (tr-am6qr), linux, debug profile** — 2026-09-18, 3 runs.

GTK numbers were **re-measured with the complete publication unit**
(PR #291 review Correction 2, thread PRRT_kwDOR1IXks6j8AJ5: the first
harness revision prebuilt the `TrackObject`s outside the timer and
timed only `display_tracks`, so `publication_first_ms` measured a
display-only slice of the startup stall — the complete unit runs
~5× slower at 1k rows and ~50× slower at 10k rows on comparable
runners). The old `publication_first_ms`/`publication_resync_ms`
metrics are retired; their quantities survive as the
`publication_display_only_ms` lower bound. The GTK table below is from
runner **gastown.toast polecat worktree (tr-am6qr), linux, debug
profile** — 2026-09-19, 2 runs per scale. Re-run the harnesses on a
different runner before trusting the numbers elsewhere.

### Engine (400-row fixture unless noted)

| Metric | Measured (3 runs) | Budget |
| --- | --- | --- |
| `startup_server_ready_ms` | 2.25 – 2.33 | < 5 |
| `startup_fullsync_ms` | 366 – 415 | < 800 |
| `startup_scan_settle_ms` | 369 – 417 | < 900 |
| `post_scan_drain_ms` | 0.65 – 0.75 | < 5 |
| `cancellation_flush_settle_ms` | 369 – 418 | < 900 |

`cancellation_flush_settle_ms` is the historical metric key for the
during-scan `Flush` barrier ack (`flush_ack_us`): it is recorded before
the benchmark aborts the engine task, so it measures startup/during-scan
settlement, not post-cancellation teardown — no post-abort settlement
is measured today.

Scaling point (`TRIBUTARY_Q4_TRACKS=4000`): `startup_fullsync_ms`
≈ 4362, `startup_scan_settle_ms` ≈ 4370 (~1.1 ms/row in debug) — scan
cost is linear in file count, so large-library behaviour is owned by
the parse-side lane's budgets, not doubled here.
`scan_progress_events` ≈ 8 at 400 rows / 80 at 4000 rows.

### GTK (1000 / 10000 rows)

GTK budgets agreed 2026-09-19 with Correction 2's re-measurement; the
display-only budgets carry over from the 2026-09-18 agreement because
that metric is the same quantity the old `publication_first_ms`
budgets were set against.

| Metric | Measured | Budget |
| --- | --- | --- |
| `fullsync_publication_first_ms` @1000 | 26.6 – 27.7 | < 45 |
| `fullsync_publication_resync_ms` @1000 | 27.1 – 31.8 | < 50 |
| `publication_display_only_ms` @1000 | 4.2 – 5.4 | < 12 |
| `browser_rebuild_ms` @1000 | 1.3 – 1.5 | < 4 |
| `fullsync_publication_first_ms` @10000 | 731 – 756 | < 1200 |
| `fullsync_publication_resync_ms` @10000 | 735 – 766 | < 1200 |
| `publication_display_only_ms` @10000 | 12.9 – 14.9 | < 40 |
| `browser_rebuild_ms` @10000 | 9.0 – 10.4 | < 30 |

The full-path budgets are wide (~60% headroom) on purpose: the point
of Correction 2 is that the stall was previously invisible, not that
~0.7 s of main-loop blockage at 10k rows (debug) is acceptable. Any
staging/deferral work that moves conversion or the folder-model
rebuild off the main loop should drive
`fullsync_publication_first_ms` toward the display-only lower bound,
and its budget can then be tightened on a fresh measurement.

### Reading

- Scan publication (`startup_fullsync_ms`) dominates startup on the
  engine side at every fixture size. On the GTK side, the display-only
  slice adds tens of ms at 10k rows (single-digit ms at 1k) in debug —
  but the **complete** publication unit the main loop actually blocks
  on is ~28 ms at 1k rows and ~0.7 s at 10k rows (debug): conversion,
  the per-source clone, and the folder-model rebuild dominate the
  stall, not the `TrackObject` splice into the track store. This gap
  is Correction 2's finding; it was invisible to the first harness
  revision.
- The during-scan settlement contract holds with margin: with the
  corrected sampler the `Flush` ack lands ~0.7 ms after `ScanComplete`
  on the engine-table runner, and the admitted command publishes at or
  after scan settlement (the benchmark asserts the non-strict
  `command_applied_us >= scan_complete_us`).
- No endpoint above misses its budget on its recorded runner today;
  the harnesses exist so the next regression or optimization is
  measured against an agreed budget instead of vibes. The engine
  budgets survived Correction 1 unchanged; the GTK budgets moved with
  Correction 2 — the old `publication_first_ms` budgets now label the
  display-only lower bound they were actually set against, and new
  full-path budgets were agreed from the re-measured values.
