# OwnTone AE1 correction

Review: `refinery-20260916-cd8f10c2-tr-t3a/corrective-instructions.md`,
rejected head `cd8f10c2f56c7aebe931cdd4a0cacaa9321820d1`.

## Finding and fix

AE1: a finite decoder EOS could enter the daemon drain wait, ignore Stop or
replacement, and publish `TrackEnded` or a synthetic drain timeout afterward.

`natural_completion` now checks the cancellation token, shared Stop gate and
pump running flag before and after each bounded daemon observation. Cancellation
returns the pump to its owning session's close path, which joins the pump and
restores the daemon or transfers retained route/lock ownership to serialized
recovery. It does not interpret a cancelled observation as completion or error.
The existing two-second HTTP timeout bounds an observation already in flight;
the polling interval is 100 ms, rather than waiting out the ten-second drain
deadline after cancellation.

Restoration stays outside the Stop gate. After restoration, terminal state and
completion/error events are published together under the existing
`SessionGate::publish_if_live` boundary, in settlement-lock-then-gate order.
Thus Stop can win during observation or restoration and suppress publication;
if publication wins, all its bounded in-memory effects precede Stop. A bare
check followed by an unguarded send is not used. Failed restoration still
retains ownership for recovery and never publishes successful completion.

## Regression coverage

Four `eos_drain_*` tests drive actual finite WAV decoding, FIFO PCM, the real
OwnTone open/pump, and the production `AirPlayOutput` controller. Test-only
instrumentation marks entry into natural EOS; the owned daemon fixture then
parks the drain HTTP observation. Stop or replacement wins before the fixture
releases either a `stop` or `play` response. Assertions cover prompt controller
return, route and instance-lock retention while observation is unsettled,
bounded restoration, restored outputs, removed takeover record, released old
route/lock, no completion/error events, and generation identity. Replacement
uses another legitimate daemon and remains playable, pausable and resumable
after the old session settles.

`natural_completion_stop_during_restore_suppresses_publication` parks the
restoring RPC after successful drain confirmation, lets Stop win, and checks
that restoration completes without any late terminal publication.

Existing normal exactly-once completion, drain-error/deadline, FIFO partial-write,
volume, generation and custody assertions remain intact. Exact-head validation
results are recorded on the source work bead at submission. Physical playback
and the wider U5 review remain outside this correction's certification.
