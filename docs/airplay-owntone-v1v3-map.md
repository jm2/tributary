# tr-t3a — OwnTone AirPlay adapter: V1–V3 corrective map

Maps every finding in the independent refinery review
(`.gc/operations/reviews/refinery-20260915-92895fd-tr-t3a/corrective-instructions.md`,
rejected head `92895fd9d3e526d8d5d7af5dff82ee4edd5f929b`) to its fix and
regression. This map supersedes the *Known gaps* section of
`docs/airplay-owntone-u1u6-map.md`; the U1–U6 dispositions there still stand.
The design contract is `docs/airplay-sender-design.md`
§4.1/§4.3/§4.4/§9.

Corrective commit on the canonical branch `polecat/tr-t3a` (base
`origin/main` `754fc6d8e6c7e7b99b1fe8844b73482f833d668d`):

| Commit | Scope |
| --- | --- |
| `92895fd9` | U1–U6 (previous leg; rejected V1–V3) |
| *(this leg)* | V1 non-blocking Stop; V2 autonomous recovery retry owner; V3 the stalled fixture's request-observed barrier |

## V1 / U3 — Stop must never block the caller behind a synchronous play RPC

- **Defect:** `SessionGate::start` held the gate mutex across the whole effect
  closure (`src/audio/airplay_sender.rs`), and `SessionGate::stop` took the
  same mutex. OwnTone's `activate_and_play` transmits `player/play` inside
  that closure (`src/audio/airplay_owntone.rs`), and the client is blocking
  with a two-second `API_TIMEOUT`. `AirplayOutput::stop`/`close_session`
  therefore waited for the stalled play RPC (and any `mutation_lock` holder)
  before it could signal cancellation — reintroducing the UI freeze the worker
  architecture exists to avoid.
- **Fix (this leg):** `SessionGate` now separates **authorization** from
  **execution**:
  - Authorization (check that no Stop has won, and reserve an in-flight slot)
    is one serialized step under the mutex, so a start is still either
    authorized *before* a Stop or refused *after* it — there is no
    check-to-effect window.
  - The effect runs **outside** the mutex, on the worker that owns it.
  - `stop()` only records cancellation and returns; it never waits for an
    in-flight effect.
  - A Stop that lands after authorization suppresses the accepted result
    (`start` returns `false`), and the worker's own session teardown settles
    the already-transmitted effect (`restore`/`player/stop` for OwnTone, the
    pipeline `Null` for GStreamer). Settlement stays worker-owned.
  - `AirplayOutput::close_session` and both adapters' `close` continue to stop
    the boundary first, so no late start is authorized.
- **Regressions:**
  - `session_gate_stop_does_not_block_behind_an_in_flight_start` — a parked
    effect and a concurrent Stop: Stop returns promptly, and the start is
    suppressed.
  - `session_gate_records_an_authorized_effect_until_settled` — an authorized
    effect still drains (settles) after a later Stop.
  - `a_stop_returns_promptly_while_the_own_tone_play_is_stalled` — a real
    stalling loopback endpoint signals that the `player/play` request reached
    the server; the Stop is then measured against a genuinely in-flight RPC
    and returns in well under the client timeout, with no accepted start.
  - The existing `session_gate_serializes_stop_and_start` and
    `a_stop_before_start_refuses_the_effect` are preserved.

## V2 / U2 — an exhausted spawn budget must not strand the retained recovery

- **Defect:** `RecoverySupervisor::enqueue` retried `ensure_worker` at most
  `SUPERVISOR_SPAWN_ATTEMPTS` times and then returned, leaving the job (and
  its held advisory lock and custodied route) in a static queue with no
  servicing thread. `ensure_worker_if_pending` was reached in production only
  from a later, unrelated `register`, so a temporary spawn failure permanently
  stranded the recovery even after the resource pressure cleared. The old
  regression supplied the missing retry owner by calling the private helper
  itself.
- **Fix (this leg):** `enqueue` now proves an owner before completing the
  handoff: `ensure_worker_until_live` retries worker creation **on the
  enqueuing recovery thread** with exponential backoff (capped) until a worker
  is live. The job stays queued (with its lock held) for the whole retry, so
  no false clean handoff is reported, and the retry path never depends on a
  future unrelated registration. This runs on a dedicated recovery/load
  worker, never on the UI Stop path.
- **Regressions:**
  - `supervisor_retains_a_retry_owner_until_the_spawn_facility_recovers` —
    injects far more spawn failures than any fixed inline budget, then
    recovers the facility externally; the production retry path starts a
    worker and the queued job is serviced and released, with no manual
    retry-helper call and no second job.
  - `supervisor_services_two_independent_jobs_fairly` is preserved.

## V3 / U6 — required behavioral fixtures

- **Fixed (this leg):** the stalled-endpoint fixture
  `a_stalled_owntone_endpoint_does_not_block_load_or_stop` now has a
  **request-observed barrier**. The hermetic fake daemon reads each accepted
  request line and appends it to the path named by
  `TRIBUTARY_FAKE_OBSERVED`; the test waits until `/api/config` has actually
  been observed at the server before it measures Stop. The test can no longer
  pass because cancellation prevented the worker from issuing `/api/config` at
  all (review V3).
- **Preserved:** every prior F1–F5 / R1–R5 / S1–S7 / T1–T6 / U1–U6 assertion
  remains; `docs/airplay-owntone-f1f5-map.md`, `-r1r5-map.md`, `-s1s7-map.md`,
  `-t1t6-map.md` and `-u1u6-map.md` still stand.

## Remaining V3 acceptance evidence (not closed at this head)

These are recorded honestly rather than claimed as resolved. Each needs an
HTTP-faithful fake OwnTone daemon (serving `/api/config`, `/api/outputs`,
`/api/player`, `/api/queue`) plus FIFO control:

1. Delayed-first-restore-then-successful-close-restore; concurrent control/EOS
   with no late effects after release (HTTP-faithful daemon).
2. Inline `airplay-owntone-recovery` thread-spawn failure injection through the
   **real** `RetainedRecovery` path with observed eventual lock/custody
   release.
3. A deterministic GStreamer pipeline Stop barrier; a real
   no-`Playing`/no-PCM/no-`TrackEnded` assertion for a failed start.
4. Replacement during a failed live close; stale `Opened` completion with exact
   route/custody counts.
5. Real FIFO EOF/drain/deadline/exactly-once F4 events, and initial-volume
   daemon-call ordering.

## Validation

Run in this worktree against the pushed branch:

- `cargo check --all-targets --locked` — exit 0
- `cargo fmt --check` — exit 0
- `cargo clippy --all-targets -- -D warnings` — exit 0
- `cargo clippy --release -- -D warnings` — exit 0
- `cargo test --all-targets` — 1929 + 30 + 20 + 1 passed, 0 failed, exit 0
