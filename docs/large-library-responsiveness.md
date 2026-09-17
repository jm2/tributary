# Large-library responsiveness measurement (Q4)

Tracking issue: <https://github.com/jm2/tributary/issues/275>

Tributary tracks end-to-end responsiveness rather than a global line-coverage
percentage. This lane provides the fixed, deterministic fixtures and the
measurement harness used to record scanner, catalogue, and command-FIFO
responsiveness at 10k and 100k tracks instead of inferring it from small
component tests.

Nothing in this lane runs in a normal `cargo test`: the fixtures are generated
on demand and the measurement test is `#[ignore]`d. No synthetic audio is
checked into the tree.

**Scope.** This lane does NOT cover the full issue-275 acceptance surface.
Time to interactive, main-loop stalls, source publication / GTK browser
rebuild, cancellation settlement, and budgets for those endpoints belong to
the explicitly split engine/UI lane (bead `tr-am6qr`, coordinated with R9
<https://github.com/jm2/tributary/issues/256>). Acceptance of the Q4 source
must not be inferred from the backend list timings recorded here.

## Fixtures

All fixtures live in the test-only module `src/local/perf_fixtures.rs`.

- **`SyntheticLibrary`** — a fixed synthetic library laid out as
  `Artist{artist:04}/Album{album:04}/Track{track:06}.wav`, with 12 tracks per
  album and 4 albums per artist. Each file is a minimal but valid 8 kHz mono WAV
  that the production `lofty` parser accepts, so the scan exercises the real
  tag-parse path rather than logging unparseable-file skips. The tree is created
  under `${TMPDIR:-/var/tmp}` and removed when the fixture is dropped.
- **`DelayedBackend<B>`** — a `MediaBackend` wrapper that adds a deterministic
  per-call latency to any inner backend and counts forwarded calls. Use it to
  model a slow remote catalogue backend without a live server.
- **Delayed parse seam** — `TEST_ONLY_PARSE_DELAY_MICROS` /
  `apply_test_only_parse_delay()` in `src/local/engine.rs`, with the
  `TEST_ONLY_PARSE_DELAY_INVOCATIONS` counter. The seam sits inside the scan's
  `spawn_blocking` parse branch and is compiled out of production builds; it is
  the same seam R9 (<https://github.com/jm2/tributary/issues/256>) needs to
  hold a scan while exercising command admission and cancellation.

## Running the measurement

```sh
TRIBUTARY_Q4_TRACKS=10000 \
TRIBUTARY_Q4_PARSE_DELAY_MICROS=100 \
TRIBUTARY_Q4_RUNNER=my-runner-label \
  cargo test --bin tributary --release -- --ignored --nocapture \
  q4_measured_large_library_responsiveness
```

Environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `TRIBUTARY_Q4_TRACKS` | `10000` | Track count to generate and measure. |
| `TRIBUTARY_Q4_RUNNER` | `$OS-$ARCH` | Human label for the reference runner. |
| `TRIBUTARY_Q4_PARSE_DELAY_MICROS` | unset | When set, run an additional scan into a **fresh second database** with this much deterministic delay per parsed file. |

Run the harness on the same named runner you intend to set budgets on. Record
the runner label and the raw numbers with the environment that produced them.

## What the harness proves

The assertions are structural, not timing-based:

- the baseline scan parses every fixture file exactly once
  (`scan_parse_invocations == TRIBUTARY_Q4_TRACKS`) and persists one row per
  file (`scan_tracks_persisted == TRIBUTARY_Q4_TRACKS`);
- the delayed pass scans the same fixture into a fresh second database, so
  every row is new and every file really enters the delayed parse branch; the
  harness asserts `delayed_parse_files_parsed == TRIBUTARY_Q4_TRACKS` and full
  persisted cardinality there too;
- the production command-FIFO leg runs the real engine loop
  (`process_library_commands_without_watcher`), enqueues 100 `SetTrackRating`
  commands plus a `Flush` barrier, and asserts the barrier is acknowledged.

## Metrics

The test prints one `Q4_METRIC name=… tracks=… value=… unit=…` line per metric
after a `Q4_ENVIRONMENT runner=…` header.

| Metric | Unit | Meaning |
| --- | --- | --- |
| `scan_tracks_persisted` | tracks | Rows committed by the initial scan. |
| `scan_parse_invocations` | parses | Files that entered the parse branch during the baseline scan. |
| `scan_elapsed` | ms | Wall time for the initial scan to settle. |
| `scan_events` | events | `LibraryEvent`s emitted during the scan. |
| `scan_throughput` | tracks/s | Persisted rows per second of scan time. |
| `backend_list_tracks` | ms | Full catalogue read through `MediaBackend`. |
| `catalogue_retained_bytes` | bytes | Estimated retained bytes of the published snapshot. |
| `backend_list_albums` | ms | Album aggregation latency. |
| `backend_list_artists` | ms | Artist aggregation latency. |
| `backend_search` | ms | Filter/search latency. |
| `backend_get_stats` | ms | Aggregate statistics latency. |
| `update_burst_count` | updates | Direct-backend rating mutations in the burst. |
| `update_burst_total` | ms | Total direct-backend burst wall time. |
| `update_burst_per_update` | ms | Mean latency per direct-backend rating mutation. |
| `command_fifo_commands` | commands | Rating commands enqueued through the production engine FIFO. |
| `command_fifo_flush_settlement` | ms | Enqueue-to-acknowledgement time for the `Flush` barrier. |
| `delayed_backend_list_tracks` | ms | `list_tracks` through `DelayedBackend`. |
| `delayed_backend_calls` | calls | Calls forwarded through the delay. |
| `delayed_parse_scan_elapsed` | ms | Fresh-database scan with the per-file parse delay applied. Includes the full cold-scan work (parse + insert), not just the delay. |
| `delayed_parse_files_parsed` | parses | Files that actually entered the delayed parse branch. |
| `delayed_parse_micros_per_file` | µs | Configured per-file delay. |
| `delayed_parse_estimated_delay_total_ms` | ms | `delayed_parse_files_parsed × micros_per_file / 1000`; the delay component inside `delayed_parse_scan_elapsed`. |

## Baseline

Raw numbers from the reference development runner `dev-linux-x86_64-polecat-dag`
(Rust release profile, in-memory SQLite, 100 µs per-file parse delay for the
delayed pass), recorded 2026-09-17 at polecat/tr-7nguk `621b56cf`:

| Metric | 10 000 tracks | 100 000 tracks |
| --- | --- | --- |
| `scan_tracks_persisted` | 10 000 tracks | 100 000 tracks |
| `scan_parse_invocations` | 10 000 parses | 100 000 parses |
| `scan_elapsed` | 8 253 ms | 56 386 ms |
| `scan_throughput` | 1 212 tracks/s | 1 773 tracks/s |
| `backend_list_tracks` | 108 ms | 809 ms |
| `catalogue_retained_bytes` | 7 030 000 bytes | 70 300 000 bytes |
| `backend_list_albums` | 8 ms | 74 ms |
| `backend_list_artists` | 7 ms | 72 ms |
| `backend_search` | 1.3 ms | 0.8 ms |
| `backend_get_stats` | 12 ms | 89 ms |
| `update_burst_per_update` | 0.24 ms | 0.12 ms |
| `command_fifo_flush_settlement` | 21 ms | 12 ms |
| `delayed_parse_scan_elapsed` | 7 926 ms | 111 359 ms |
| `delayed_parse_files_parsed` | 10 000 parses | 100 000 parses |
| `delayed_parse_estimated_delay_total_ms` | 1 000 ms | 10 000 ms |

The baseline scan is dominated by fixture-driven row insertion and root
enrollment. The delayed pass repeats the full cold scan into a fresh database
(parse + insert for every file) with the configured delay added per file, so
compare `delayed_parse_scan_elapsed` against the sum of a cold scan and
`delayed_parse_estimated_delay_total_ms`, not against the delay alone.

These are development-runner observations, not budgets. Pass/fail budgets are
agreed per named runner in the engine/UI lane before any failing path is
optimized (see below).

## Budgets

Pass/fail budgets are **not** set yet. They are to be agreed per named runner
(before any failing path is optimized) and fail only where a measurement or a
concrete code finding shows the budget is missed. Module line counts alone do
not justify a rewrite.

## Explicit scoped split

The remaining issue-275 acceptance metrics require the running engine/UI and
are split out — they are not deferred silently:

- **Owned by this lane** (implemented above): fixed 10k/100k fixtures,
  delayed filesystem/parser seam with invocation-count proof, backend
  source/filter/rebuild/search/stats latencies, retained catalogue bytes,
  update bursts, production command-FIFO admission/`Flush` settlement.
- **Owned by the engine/UI lane** (bead `tr-am6qr`, coordinated with R9
  <https://github.com/jm2/tributary/issues/256>): time to interactive,
  main-loop stalls during interactive startup, source publication / GTK
  browser rebuild latency, cancellation settlement of an in-flight scan, and
  runner-specific budgets for those endpoints.

Prerequisites for the engine/UI lane are independently owned: R9 #256
held-parse seam coordination and a GTK-capable reference runner with agreed
labels and budgets. Acceptance of the Q4 source must not be inferred from the
backend timings in this document alone.
