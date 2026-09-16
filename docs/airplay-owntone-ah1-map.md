# OwnTone AH1 corrective mapping

The refinery rejected `1a419e0fbc4e8a0b679a96edc755005e1357b4f4` because
live pause/volume failures were logged and discarded while uncertain mutations
remained active and later controls could overtake them.

## AH1: terminal live controls and automatic settlement

The sender seam returns a success boolean for pause and volume. OwnTone runs
these controls through the existing mutation boundary with terminal failure
handling enabled. A failed RPC latches terminal and stops the pump before
releasing that boundary, preserving the outstanding mutation count. Error and
Stopped publication run under the Stop gate, with cancellation suppressed.
The worker consumes false, stops accepting queued commands, and invokes its
existing close/restoration/recovery path off GTK. Quiescence must settle the
uncertain mutation before lock, takeover record and protected route release.
The GStreamer adapter preserves its existing control behavior.

## Production-controller regressions

Eight real WAV/FIFO controller tests cover pause and volume independently:
HTTP 500, a parked mutation surviving the actual client timeout, Stop before
HTTP failure, and Stop before timeout. The ordinary failure cases issue no
Stop, replacement, drop or UI-triggered cleanup until automatic settlement.
All cases queue play, pause and volume while the original request is parked
and assert that none transmits. They verify:

- Immediate controller return and retained lock, record, selected output and
  protected route while the mutation is in flight.
- Generation-scoped Error and Stopped for live failures, silent Stop-first
  cancellation, and no late Playing, Paused or TrackEnded.
- Automatic daemon quiescence/restart, prior output restoration, stopped
  playback, cleared record/custody and released route/lock.
- Direct-source PlaybackSession semantics, inert controls after settlement,
  and a usable subsequent load on the same controller.

All prior assertions are retained. Exact-head check results are recorded on
the source bead at submission. Physical playback and wider U5 remain outside
this correction's validation.
