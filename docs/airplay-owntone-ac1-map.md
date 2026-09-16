# OwnTone AC1 corrective map

Corrective report: `refinery-20260916-ffd1d6cf-tr-t3a/corrective-instructions.md`.
Rejected head: `ffd1d6cf0f1fb43f88708a6cc3013c5c8ef67d36`.

## Startup protocol

Initial activation uses pipe autostart. `open` still clears the dedicated queue,
selects the output and applies initial volume before activation. It creates an
inert decode pipeline and its descriptor-owning pump. `activate_and_play` now
starts that pipeline inside the current generation's `SessionGate` and the
session's mutation settlement boundary. It polls the bounded player API until
the daemon reports `play`, then publishes Playing and releases the pump to
observe the already-running pipeline. It does not send ordinary player/play
against the empty queue. Later resume continues to use ordinary player/play.

Stop winning the gate prevents pipeline startup and PCM. If activation already
owns the gate, Stop remains prompt, the startup poll notices cancellation, and
teardown settles the authorized effect before releasing the route or instance
lock. Failed activation stops decoding before releasing the activation mutex,
so the pump cannot close its descriptor while the decoder is still writing.
A bounded in-memory publication under the Stop gate prevents a late daemon
response from publishing Playing after Stop. Failure never publishes Playing.
Uncertain daemon effects retain the existing
quiescence/recovery mechanism. The pump cannot independently restart decoding.

The protocol follows OwnTone 29.3 commit
`d6fb3edf5831de38134ebd92fcf09a730ddd37aa`:

- [JSON API](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/httpd_jsonapi.c):
  queue clear stops and empties playback; ordinary play propagates startup
  failure as HTTP 500.
- [Player](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/player.c):
  `playback_start` fails without a queue item, while `playback_start_id` creates
  the queue entry for the scanned pipe. `playback_start_bh` publishes play state.
- [Pipe input](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/inputs/pipe.c):
  `pipe_read_cb` starts the scanned pipe by ID when PCM becomes readable.
  `setup` opens nonblocking; `play` autostops on EOF only for an autostarted pipe.
  Preserving this behavior lets finite PCM complete through the existing
  daemon-confirmed stop, restore and TrackEnded path.

## Behavioral regressions

The owned recording daemon now starts stopped with an empty queue. Queue clear
empties it, ordinary play fails without an item, and opening the FIFO writer
alone cannot autostart. Only actual PCM makes the scanned pipe playable. The
fixture reads the real FIFO, reports consumed bytes, models autostart EOF, and
records the first PCM independently of HTTP calls. It retains strict volume
query validation and rejects unknown HTTP operations.

The `empty_queue_start_*` tests exercise production `OwnToneSender::open_session`,
real GStreamer decoding of usable WAV through a protected loopback route, and
`run_session_worker`. A sender wrapper parks only after real open, allowing Stop
to win the first-effect gate deterministically without replacing activation.
Coverage includes:

- Successful first playback, daemon play state, nonzero PCM, worker cache,
  initial volume, changed output selection, held instance lock and live media
  ticket. Teardown verifies the prior output selection is restored.
- Stop before first effect: no PCM, no play request, no Playing or TrackEnded,
  followed by restoration and route/lock release.
- Stop during autostart: Stop returns promptly after actual PCM arrives,
  suppresses Playing, and settles playback before releasing ownership.
- Failed pipe autostart after PCM: no Playing or TrackEnded, terminal worker
  state, settlement/restoration and route/lock release.
- Finite PCM: exactly one TrackEnded after Playing, terminal cache observed
  while the worker is still live, and no Playing after terminal publication.

The initial/live-volume test retains 42/73 and request-order assertions, now
uses usable WAV and the daemon's FIFO reader, and verifies PCM autostart before
exercising pause/resume. The prior real-FIFO drain fixture now activates only after installing its
pipeline, and reports play only after consuming PCM; all its drain, event and
restoration assertions remain. Existing cancellation, terminal publication, recovery
custody and instance exclusivity tests remain in place.

## Scope and validation

The stateful fixture is an upstream-contract regression, not an actual OwnTone
instance or physical receiver. Wider U5 authority/runtime certification and
physical playback remain separate. Exact-head quality gates and test outcomes
are recorded on the source bead at submission; independent refinery review is
still required.
