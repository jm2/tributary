# Large-library responsiveness measurement (Q4)

Tracking issue: <https://github.com/jm2/tributary/issues/275>

Tributary tracks end-to-end responsiveness rather than a global line-coverage
percentage. This lane provides the fixed, deterministic fixtures and the
measurement harness used to record scanner, catalogue, and command-FIFO
responsiveness at 10k and 100k tracks instead of inferring it from small
component tests.

The 10k/100k measurement test is `#[ignore]`d: nothing in this lane generates
a large library in a normal `cargo test`. A small always-on companion test
(`q4_fixture_scan_produces_real_catalogue_fan_out`, 100 tracks) does run in
every `cargo test` to guard the fixture/catalogue contract. No synthetic audio
is checked into the tree.

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
  that the production `lofty` parser accepts **and that carries ID3v2.3
  metadata matching its directory position** (`TIT2`/`TPE1`/`TALB`/`TRCK`
  written into a RIFF `id3` chunk (space-padded id), so the scan persists genuinely distinct
  artists and albums. This matters because the production scan persists what
  the tag parser reads and the backend aggregates on those persisted values —
  not on directory names. An earlier revision wrote the same untagged payload
  into every file, which collapsed the whole catalogue into one
  `Unknown Artist` / `Unknown Album` row pair and made every album/artist
  measurement a single-row fan-out; that shape is invalid for the intended
  catalogue. The tree is created under `${TMPDIR:-/var/tmp}` and removed when
  the fixture is dropped.
- **`DelayedBackend<B>`** — a `MediaBackend` wrapper that adds a deterministic
  per-call latency to any inner backend and counts forwarded calls. Use it to
  model a slow remote catalogue backend without a live server.
- **Delayed parse seam** — `TEST_ONLY_PARSE_DELAY_MICROS` /
  `apply_test_only_parse_delay()` in `src/local/engine.rs`, with the
  `TEST_ONLY_PARSE_DELAY_INVOCATIONS` counter. The seam sits inside the scan's
  `spawn_blocking` parse branch and is compiled out of production builds; it is
  the same seam R9 (<https://github.com/jm2/tributary/issues/256>) needs to
  hold a scan while exercising command admission and cancellation. Because
  both opt-in Q4 tests share the process-wide seam in one test binary, every
  delay window (and the startup benchmark's engine run) holds the
  `TEST_ONLY_PARSE_DELAY_WINDOW` mutex, so parallel `--ignored` runs cannot
  arm the seam under each other or pollute each other's counters.

## Running the measurement

```sh
TRIBUTARY_Q4_LIBRARY_TRACKS=10000 \
TRIBUTARY_Q4_PARSE_DELAY_MICROS=100 \
TRIBUTARY_Q4_RUNNER=my-runner-label \
  cargo test --bin tributary --release -- --ignored --nocapture \
  q4_measured_large_library_responsiveness
```

Environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `TRIBUTARY_Q4_LIBRARY_TRACKS` | `10000` | Track count to generate and measure. |
| `TRIBUTARY_Q4_RUNNER` | `$OS-$ARCH` | Human label for the reference runner. |
| `TRIBUTARY_Q4_PARSE_DELAY_MICROS` | unset | Per-file parse delay in µs for the delayed scan. |

Setting `TRIBUTARY_Q4_PARSE_DELAY_MICROS` runs an additional scan into a **fresh second
database** with that much deterministic delay per parsed file.

`TRIBUTARY_Q4_LIBRARY_TRACKS` is deliberately distinct from the engine
startup benchmark's `TRIBUTARY_Q4_TRACKS` (default 400, see
`docs/engine-ui-responsiveness.md`): a shared variable with different
defaults (10 000 here) would let one measurement run silently resize the
other's fixture.

Run the harness on the same named runner you intend to set budgets on. Record
the runner label and the raw numbers with the environment that produced them.

## What the harness proves

The assertions are structural, not timing-based:

- the baseline scan parses every fixture file exactly once
  (`scan_parse_invocations == TRIBUTARY_Q4_LIBRARY_TRACKS`) and persists one row per
  file (`scan_tracks_persisted == TRIBUTARY_Q4_LIBRARY_TRACKS`);
- **catalogue fan-out is asserted before any metric is recorded**: through the
  real backend, 10 000 tracks must yield 834 distinct albums across 209
  artists, and 100 000 tracks 8 334 albums across 2 084 artists (12 tracks per
  album, 4 albums per artist, documented final partial groups — the same
  numbers `expected_album_count` / `expected_artist_count` compute);
- the delayed pass scans the same fixture into a fresh second database, so
  every row is new and every file really enters the delayed parse branch; the
  harness asserts `delayed_parse_files_parsed == TRIBUTARY_Q4_LIBRARY_TRACKS`, full
  persisted cardinality there too, **and the same album/artist fan-out**;
- the production command-FIFO leg runs the real engine loop
  (`process_library_commands_without_watcher`), enqueues 100 `SetTrackRating`
  commands plus a `Flush` barrier, and asserts the barrier is acknowledged;
- an always-on companion test (`q4_fixture_scan_produces_real_catalogue_fan_out`)
  scans a 100-track fixture in every `cargo test` run and asserts the same
  fan-out at small scale (9 albums, 3 artists) plus per-row attribution, so a
  regression cannot reach the ignored measurement silently.

## Metrics

The test prints one `Q4_METRIC name=… tracks=… value=… unit=…` line per metric
after a `Q4_ENVIRONMENT runner=…` header.

| Metric | Unit | Meaning |
| --- | --- | --- |
| `scan_tracks_persisted` | tracks | Rows committed by the initial scan. |
| `scan_parse_invocations` | parses | Files that entered the parse branch in the baseline scan. |
| `scan_albums` | albums | Distinct albums the backend reports after the scan. |
| `scan_artists` | artists | Distinct artists the backend reports after the scan. |
| `scan_elapsed` | ms | Wall time for the initial scan to settle. |
| `scan_events` | events | `LibraryEvent`s emitted during the scan. |
| `scan_throughput` | tracks/s | Persisted rows per second of scan time. |
| `backend_list_tracks` | ms | Full catalogue read through `MediaBackend`. |
| `catalogue_retained_bytes` | bytes | Estimated retained bytes of the published snapshot (fixed `Track` size plus every heap-owned string, including the backend-native track id and any `stream_url`/`cover_art_url`). |
| `backend_list_albums` | ms | Album aggregation latency. |
| `backend_list_artists` | ms | Artist aggregation latency. |
| `backend_search` | ms | Filter/search latency. |
| `backend_get_stats` | ms | Aggregate statistics latency. |
| `update_burst_count` | updates | Direct-backend rating mutations in the burst. |
| `update_burst_total` | ms | Total direct-backend burst wall time. |
| `update_burst_per_update` | ms | Mean latency per direct-backend rating mutation. |
| `command_fifo_commands` | commands | Rating commands enqueued through the production FIFO. |
| `command_fifo_flush_settlement` | ms | Enqueue-to-acknowledgement time for the `Flush` barrier. |
| `delayed_backend_list_tracks` | ms | `list_tracks` through `DelayedBackend`. |
| `delayed_backend_calls` | calls | Calls forwarded through the delay. |
| `delayed_parse_scan_elapsed` | ms | Delayed fresh-DB scan including the cold-scan work. |
| `delayed_parse_files_parsed` | parses | Files that actually entered the delayed parse branch. |
| `delayed_parse_micros_per_file` | µs | Configured per-file delay. |
| `delayed_parse_estimated_delay_total_ms` | ms | Delay share of `delayed_parse_scan_elapsed`. |

The estimated delay total is `delayed_parse_files_parsed × micros_per_file / 1000`, the delay
component of `delayed_parse_scan_elapsed`.

## Baseline

**Invalid baseline, kept for the record:** the numbers recorded 2026-09-17 at
`polecat/tr-7nguk` `621b56cf` on `dev-linux-x86_64-polecat-dag` were taken over
the collapsed catalogue (every fixture file untagged, so all rows shared one
`Unknown Artist` / `Unknown Album` pair). Their `backend_list_albums` /
`backend_list_artists` figures measured a one-result aggregation, not the
intended 834/209 (10k) and 8 334/2 084 (100k) fan-out, and must not be cited
as Q4 measurements.

Current baseline, recorded over the fanned-out tagged catalogue on the
reference development runner `dev-linux-x86_64-polecat-rictus` (Rust release
profile, in-memory SQLite, 100 µs per-file parse delay for the delayed pass):

| Metric | 10 000 tracks | 100 000 tracks |
| --- | --- | --- |
| `scan_tracks_persisted` | 10 000 tracks | 100 000 tracks |
| `scan_parse_invocations` | 10 000 parses | 100 000 parses |
| `scan_albums` | 834 albums | 8 334 albums |
| `scan_artists` | 209 artists | 2 084 artists |
| `scan_elapsed` | 5 512 ms | 59 629 ms |
| `scan_throughput` | 1 814 tracks/s | 1 677 tracks/s |
| `backend_list_tracks` | 80 ms | 896 ms |
| `catalogue_retained_bytes` | 6 950 000 bytes | 69 500 000 bytes |
| `backend_list_albums` | 7.1 ms | 113 ms |
| `backend_list_artists` | 8.4 ms | 105 ms |
| `backend_search` | 0.8 ms | 0.9 ms |
| `backend_get_stats` | 9.6 ms | 118 ms |
| `update_burst_per_update` | 0.12 ms | 0.14 ms |
| `command_fifo_flush_settlement` | 12 ms | 16 ms |
| `delayed_parse_scan_elapsed` | 8 441 ms | 66 826 ms |
| `delayed_parse_files_parsed` | 10 000 parses | 100 000 parses |
| `delayed_parse_estimated_delay_total_ms` | 1 000 ms | 10 000 ms |

Recorded 2026-09-17 at `polecat/tr-7nguk` `cbf1d710`. Album/artist aggregation
now costs real work over real fan-out (834→8 334 album groups, 209→2 084
artist groups) — compare against the invalid collapsed-catalogue baseline
above only to see what the collapsed shape hid, never as a regression
reference.

`catalogue_retained_bytes` in this table predates the metric fix that now
counts the heap-native track id (and any `stream_url`/`cover_art_url`):
with ~36-byte ids it underreported roughly 3.6 MB per 100 000 rows, so
re-record the metric on your runner before comparing against these figures
or setting byte budgets on them.

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
are split out — they are not deferred silently. The split is executable, not
prose: the split bead carries scoped acceptance criteria, prerequisite
records, and an execution lane, and a dependency edge keeps the overall task
incomplete until it lands.

- **Owned by this lane** (implemented above): fixed 10k/100k tagged fixtures,
  delayed filesystem/parser seam with invocation-count proof, backend
  source/filter/rebuild/search/stats latencies, retained catalogue bytes,
  update bursts, production command-FIFO admission/`Flush` settlement.
  Known scope limit: the FIFO leg enqueues its commands after the measured
  scan completes, so it proves post-scan admission/settlement only — command
  admission **during** an initial scan is explicitly owned by the engine/UI
  lane below.
- **Owned by the engine/UI lane**: bead `tr-am6qr` (open in the tributary
  ledger, execution lane `tributary/gastown.polecat`, split of this source,
  coordinated with R9 <https://github.com/jm2/tributary/issues/256>). Its
  acceptance criteria enumerate: time to interactive and main-loop stalls
  during interactive startup, source publication / GTK browser rebuild
  latency after scan settle, cancellation settlement of an in-flight scan
  including during-scan command admission, and runner-specific budgets for
  those endpoints agreed before any failing path is optimized.
- **Dependency edges**: `tr-am6qr` blocks `tr-7nguk` in the tributary ledger
  (`gc bd dep add tr-7nguk tr-am6qr`). The Q4 source stays open as the
  integration owner for issue #275; merging this branch does not complete
  Q4 while the engine/UI endpoints remain undelivered.

Prerequisites for the engine/UI lane are recorded on `tr-am6qr` as
independently owned records: R9 #256 held-parse seam coordination and a
GTK-capable reference runner with an agreed label. Budgets are never invented
by this lane; acceptance of the Q4 source must not be inferred from the
backend timings in this document alone.
