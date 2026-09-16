# OwnTone AG1 corrective mapping

The refinery rejected `9acd42dc7a677f632b68ae59b55570e8fe672775` because
the live worker discarded a failed Resume result and retained the session
until a separate Stop or replacement. Direct-source UI error handling does
not guarantee either operation.

## AG1: worker-owned terminal teardown after failed Resume

`run_session_worker` consumes the live Resume result. On false it marks the
cache Stopped, exits the command loop, and invokes the same worker-owned
`session.close()` used by terminal control paths. No subsequent queued control
can revive the failed session. Blocking decoder shutdown, pump join,
restoration and recovery remain off GTK. The adapter's existing Error and
Stopped publication, AF1 silent cancellation, mutation accounting, mandatory
quiescence, exclusive lock and keyed route custody are preserved.

## Regression coverage

`live_resume_http_failure_settles_without_ui_stop` and
`live_resume_timeout_settles_without_ui_stop` use the production controller,
real protected WAV route, decoder/FIFO and owned request-recording daemon.
They first play and pause successfully, then park the live resume PUT.
One releases an HTTP 500; the other leaves the request parked through the
actual HTTP timeout. Neither issues Stop, replacement or controller drop
until automatic settlement has completed. The fixtures verify:

- Prompt control return and retained route, takeover record, selected output
  and exclusive lock while the request is in flight.
- Automatic settlement, daemon process replacement (quiescence), restored
  prior outputs and stopped daemon, deleted takeover record, released lock
  and route, and no remaining recovery custody.
- Visible Error and Stopped, no TrackEnded or late Playing, and inert later
  Play/Pause commands after the terminal session worker has exited.
- The real PlaybackSession direct-source failure predicate remains false and
  the generation remains current, so conditional UI cleanup cannot mask this.
- A fresh load on the same controller and dedicated daemon can play, pause,
  resume and stop successfully after settlement.

All prior assertions are retained. Exact-head checks and results are recorded
in the source bead on submission. Physical playback and wider U5 remain
outside this correction's validation.
