# Large-library responsiveness measurement (Q4)

Tracking issue: <https://github.com/jm2/tributary/issues/275>

Tributary tracks end-to-end responsiveness rather than a global line-coverage
percentage. This lane provides the fixed, deterministic fixtures and the
measurement harness used to record that responsiveness, so scanner, catalogue
publication, browser rebuild, and command admission can be compared at 10k and
100k tracks instead of being inferred from small component tests.

Nothing in this lane runs in a normal `cargo test`: the fixtures are generated
on demand and the measurement test is `#[ignore]`d. No synthetic audio is checked
into the tree.

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
  `apply_test_only_parse_delay()` in `src/local/engine.rs`. It injects a fixed
  per-file delay inside the scan's `spawn_blocking` parse step. It is compiled
  out of production builds and is the same seam R9
  (<https://github.com/jm2/tributary/issues/256>) needs to hold a scan while
  exercising command admission and cancellation.

## Running the measurement

```sh
TRIBUTARY_Q4_TRACKS=10000 \
TRIBUTARY_Q4_RUNNER=my-runner-label \
  cargo test --bin tributary --release -- --ignored --nocapture \
  q4_measured_large_library_responsiveness
```

Environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `TRIBUTARY_Q4_TRACKS` | `10000` | Track count to generate and measure. |
| `TRIBUTARY_Q4_RUNNER` | `$OS-$ARCH` | Human label for the reference runner. |
| `TRIBUTARY_Q4_PARSE_DELAY_MICROS` | unset | When set, run a second scan with this much deterministic delay per parsed file. |

Run the harness on the same named runner you intend to set budgets on. Record
the runner label and the raw numbers with the environment that produced them.

## Metrics

The test prints one `Q4_METRIC name=… tracks=… value=… unit=…` line per metric
after a `Q4_ENVIRONMENT runner=…` header.

| Metric | Unit | Meaning |
| --- | --- | --- |
| `scan_tracks_persisted` | tracks | Rows committed by the initial scan. |
| `scan_elapsed` | ms | Wall time for the initial scan to settle. |
| `scan_events` | events | `LibraryEvent`s emitted during the scan. |
| `scan_throughput` | tracks/s | Persisted rows per second of scan time. |
| `backend_list_tracks` | ms | Full catalogue read through `MediaBackend`. |
| `catalogue_retained_bytes` | bytes | Estimated retained bytes of the published snapshot. |
| `backend_list_albums` | ms | Album aggregation latency. |
| `backend_list_artists` | ms | Artist aggregation latency. |
| `backend_search` | ms | Filter/search latency. |
| `backend_get_stats` | ms | Aggregate statistics latency. |
| `update_burst_count` | updates | Rating mutations in the burst. |
| `update_burst_total` | ms | Total burst wall time. |
| `update_burst_per_update` | ms | Mean latency per rating mutation. |
| `delayed_backend_list_tracks` | ms | `list_tracks` through `DelayedBackend`. |
| `delayed_backend_calls` | calls | Calls forwarded through the delay. |
| `delayed_parse_scan_elapsed` | ms | Second scan with the parse delay applied. |
| `delayed_parse_micros_per_file` | µs | Configured per-file delay. |

## Baseline

Raw numbers from a development smoke run (not a budget and not a release
runner) of the `10 000`-track fixture in the debug test profile on
`dev-linux-x86_64`, with a 100 µs per-file parse delay for the delayed pass:

| Metric | Value |
| --- | --- |
| `scan_elapsed` | 8842 ms |
| `scan_throughput` | 1131 tracks/s |
| `backend_list_tracks` | 323 ms |
| `catalogue_retained_bytes` | 7 040 000 bytes |
| `backend_list_albums` | 20 ms |
| `backend_list_artists` | 19 ms |
| `backend_search` | 3 ms |
| `backend_get_stats` | 26 ms |
| `update_burst_per_update` | 0.42 ms |
| `delayed_parse_scan_elapsed` | 690 ms |

The first scan is dominated by fixture-driven row insertion and root
enrollment; the delayed second pass re-traverses and re-parses the same files
but commits no new rows, so it isolates the per-file traversal/parse cost.

## Budgets

Pass/fail budgets are **not** set yet. They are to be agreed per named runner
(before any failing path is optimized) and fail only where a measurement or a
concrete code finding shows the budget is missed. Module line counts alone do
not justify a rewrite.

## Deferred metrics

The following acceptance metrics require a running engine/UI and are
coordinated with R9 (#256) rather than duplicated here:

- time to interactive and main-loop stalls during interactive startup;
- cancellation settlement;
- command admission during the initial scan.

The fixtures and the delayed parse seam above are the shared substrate for that
work.
