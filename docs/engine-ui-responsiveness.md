# Engine / UI responsiveness measurement lane (Q4)

Scope: the **engine-loop and GTK-responsiveness** half of issue #275
(bead `tr-am6qr`). This lane measures, per runner:

- engine startup time-to-interactive components (server ready, scan
  full-sync publication, scan settlement),
- settlement of an in-flight scan, including a library command and the
  shutdown `Flush` barrier admitted **during** the scan,
- the GTK-side cost of publishing a scan result (source publication,
  browser rebuild), which is simultaneously the **main-loop stall**
  the UI thread suffers, because `display_tracks` runs synchronously
  on the main loop.

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
`publication_first_ms` (GTK side below).

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
| `publication_first_ms` | `display_tracks` 0 → N rows on an empty browser (startup FullSync path) |
| `publication_resync_ms` | `display_tracks` N → N rows (idempotent republication) |
| `browser_rebuild_ms` | `rebuild_browser_data` alone at N rows |

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
worktree (tr-am6qr), linux, debug profile** — 2026-09-18, 3 runs. The
GTK table keeps the original **gastown.furiosa** numbers: the sampler
defect touched only the engine timeline, not the `display_tracks`
measurement path. Re-run the harnesses on a different runner before
trusting the numbers elsewhere.

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

| Metric | Measured | Budget |
| --- | --- | --- |
| `publication_first_ms` @1000 | 4.9 | < 12 |
| `publication_resync_ms` @1000 | 1.9 | < 6 |
| `browser_rebuild_ms` @1000 | 1.3 | < 4 |
| `publication_first_ms` @10000 | 14.5 – 17.2 | < 40 |
| `publication_resync_ms` @10000 | 14.4 – 15.9 | < 40 |
| `browser_rebuild_ms` @10000 | 10.8 – 11.2 | < 30 |

### Reading

- Scan publication (`startup_fullsync_ms`) dominates startup on the
  engine side at every fixture size; GTK publication adds only tens of
  ms at 10k rows (single-digit ms at 1k) in debug.
- The during-scan settlement contract holds with margin: with the
  corrected sampler the `Flush` ack lands ~0.7 ms after `ScanComplete`
  on the engine-table runner, and the admitted command publishes at or
  after scan settlement (the benchmark asserts the non-strict
  `command_applied_us >= scan_complete_us`).
- No endpoint above misses its budget on its recorded runner today;
  the harnesses exist so the next regression or optimization is
  measured against an agreed budget instead of vibes. All budgets
  still hold with the corrected sampler — none had to move.
