# OwnTone AI1 corrective mapping

The refinery rejected `634a2f6f34538e7eed1502928fcf169bf1108052` because
pump termination did not cause the command worker to close its session.
Decoder errors and completion restoration failures could retain the instance
lock and uncertain takeover forever when the direct-source UI sent no Stop.
Even successful finite completion left the worker holding the instance lock.

## Explicit pump completion and worker-owned settlement

`SenderSession::is_finished` supplies a nonblocking terminal predicate distinct
from observed daemon Stopped. OwnTone implements it using the pump join handle's
completion state. The command worker checks it on its existing bounded polling
loop and enters its existing close/restoration/recovery path off GTK.

Waiting for actual pump completion, rather than the early terminal latch in
`restore`, preserves terminal event publication: automatic close cannot stop
the publication gate while normal EOS is still restoring and publishing.
Failed restoring mutations retain their outstanding count and force daemon
quiescence before route/record/lock release. The worker retains registration
and session ownership through cleanup, including supervised recovery.

Decoder-error publication now uses the mutation boundary and shared Stop gate
for Error/Stopped together, suppressing intentional cancellation. Live control
refusal, uncertain mutation accounting, bounded teardown and recovery custody
are unchanged.

## Production-controller regressions

Six tests drive the actual controller, OwnTone open, PCM pipeline, FIFO, pump
and worker. Decoder failure comes from closing the real FIFO reader after
Playing, causing the actual fdsink to report a bus error. Finite EOS comes from
a one-second WAV. Each path covers successful restoration, HTTP 500 restoration,
and a restoring stop request held beyond the actual HTTP timeout. Faults are
one-shot, permitting recovery only after the old daemon is quiesced.

Before settlement no test sends Stop, replacement or UI cleanup or drops the
controller. Assertions cover retained route, record, output and lock during
in-flight restoration; automatic settlement; required process replacement for
uncertain mutations; restored prior outputs; cleared record/custody; released
route and lock; generation-scoped terminal events without false TrackEnded or
late Playing/Paused; inert late controls; direct-source UI semantics; and a
usable subsequent load on the same controller. Clean finite EOS emits exactly
one TrackEnded and releases the instance lock independently of UI advancement.

All existing assertions are preserved. Exact-head validation is recorded on
the source bead at submission. Physical playback and wider U5 remain outside
this correction's validation.
