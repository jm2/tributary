# OwnTone AF1 corrective mapping

The refinery rejected `0b691541c6e05759b1575b1fb61d62850a25c9ab` because
`activate_and_play` emitted an Error when Stop cancelled first PCM/autostart
or won against an in-flight resume. Stop retains the event generation, so
replacement-generation filtering alone cannot make this silent.

## AF1: silent cancellation, atomic failure publication

`SessionInner::activate_and_play` now takes the settlement boundary and uses
`SessionGate::publish_if_live` for the failure decision and Error send together.
Cancelled or terminal activation returns `false` to the production worker
without publishing an Error; the worker closes the session. A genuine failure
in a live session still emits an Error. Stop can win before publication or
follow the complete bounded publication; it cannot interleave a check and send.
The lock order remains settlement boundary then Stop gate, and no network or
pipeline work runs under the Stop gate. The existing outstanding mutation count,
decoder shutdown, restoration, recovery custody and instance lock are unchanged.

## Regression coverage

Five `cancelled_activation_*_is_silent` tests use the production controller,
real protected WAV route, real decoder/FIFO and owned request-recording daemon:

- First PCM/autostart observation, followed by Stop or replacement, with the
  successful observation released only after cancellation.
- A paused session's resume PUT, followed by Stop or replacement, with the
  successful RPC released only after cancellation.
- A failing resume PUT released after Stop, also remaining silent.

The daemon parks precisely the selected request type. Assertions cover prompt
controller return, retained route and exclusive lock while unsettled, bounded
restoration and release, restored outputs, no takeover record or recovery
custody, no Error/TrackEnded or late Playing from the cancelled generation,
and usable replacement play/pause/resume/stop. Existing startup tests retain
every assertion and additionally require no Error for StopBeforePcm,
StopDuringAutostart and StalledStartupStop, and require an Error for genuine
FailAutostart and StalledTimeout failures.

Exact-head commands and results are recorded in the source bead at submission.
This correction does not certify wider U5 or physical playback.
