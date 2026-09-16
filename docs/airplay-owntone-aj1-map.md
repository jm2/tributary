# OwnTone AJ1 corrective mapping

The refinery rejected `6c1f8f1d8d4addb913a30e1b31fe27e482daf2a1` because
volume/pause refusal during completion restoration could send the worker into
close before the pump published its terminal outcome. Stopping the event gate
there suppressed successful TrackEnded or a genuine restoration Error/Stopped.

## Correction

OwnTone close now preserves the event gate when the session already latched
terminal. It stops decoding and joins the pump before stopping that gate, then
runs the existing restoration/recovery path. Thus a terminal control refusal
still triggers worker-owned settlement, but cannot cancel pending terminal
publication. Controls remain refused after restoration; this does not reinterpret
a refused control as a successful mutation. The same ordering covers resume
refusal and automatic no-command close.

Explicit controller Stop/replacement still cancels the token and stops the shared
gate immediately, so intentional cancellation suppresses pending outcomes even
while the worker joins. Close of a nonterminal session still stops the gate first.
Genuine live mutation failures retain their terminal latch, error publication,
outstanding mutation accounting, quiescence, custody and automatic cleanup.
No blocking work moves to GTK and no ownership is released before settlement.

## Deterministic production-controller coverage

Seven new tests reuse the real finite WAV/FIFO/pump/controller settlement fixture.
A restoring stop RPC is parked while volume/pause is queued; a probe observes the
control reaching the mutation boundary. A separate barrier parks the pump after
restore returns and before publication. The test waits until the refused control
has driven the worker into close/join before releasing terminal publication.
Resume refusal also exercises this ordering without transmitting a play RPC.

Successful volume/pause/resume cases require exactly one TrackEnded. Failed
restoration with volume/pause requires genuine Error/Stopped and no TrackEnded.
Two Stop-first cases cancel while publication is parked, covering successful
and failed restoration. No refused pause/play/volume reaches the daemon. The
normal cases send no Stop/replacement/drop or UI cleanup to cause settlement.

The shared fixture preserves all six AI1 cases and their assertions: retained
in-flight route/lock/record, automatic resource settlement, process replacement
for uncertain restoring mutations, restored outputs, cleared record/custody,
generation-correct events, no late Playing/Paused, direct-source UI semantics,
inert late controls, and a usable subsequent load. Existing AE1/AF1/AH1
cancellation and failure regressions remain intact.

Exact-head validation is recorded on the source bead at submission. Physical
playback and wider U5 remain outside this correction's validation.
