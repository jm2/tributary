# OwnTone AD1 corrective map

Report: `refinery-20260916-64e4c478-tr-t3a/corrective-instructions.md`.
Rejected head: `64e4c478f51db73df1bb4a2f2edf002cf75408ba`.

## Finding and correction

AD1 identified a blocking FIFO write that could prevent pipeline shutdown from
reaching restoration or retained recovery. `open_pipe_write` now preserves
`O_NONBLOCK` for the entire descriptor lifetime. The existing `fdsink` handles
partial writes and retries EAGAIN through its cancellable poll. A stalled reader
therefore applies backpressure without trapping a streaming task in a kernel
write. Shutdown joins the streaming task before its owner releases the descriptor;
no concurrent close, descriptor reuse, or terminal EAGAIN workaround is introduced.

The matching GStreamer 1.28.7 implementation is documented in
[gstelements_private.c](https://github.com/GStreamer/gstreamer/blob/1.28.7/subprojects/gstreamer/plugins/elements/gstelements_private.c)
and [gstfdsink.c](https://github.com/GStreamer/gstreamer/blob/1.28.7/subprojects/gstreamer/plugins/elements/gstfdsink.c).
Its write loop retries transient backpressure and advances partially written
buffers; sink unlock flushes its poll. Keeping the OS write nonblocking lets
shutdown reach that cancellation path.

## Regressions

- `oversized_fifo_write_is_interruptible_after_partial_progress` measures a real
  FIFO's capacity, submits one buffer four times that capacity through `fdsink`,
  and waits until the non-consuming reader observes a full pipe. It verifies no
  premature error/EOS, then requires Null to finish within two seconds while the
  reader remains open and the descriptor owner remains alive through shutdown.
- `oversized_fifo_write_resumes_without_losing_pcm` starts with the same oversized
  buffer and full pipe, then consumes PCM. It compares every byte in order and
  requires EOS, covering successful partial-write retries rather than cancellation
  alone.
- `stalled_fifo_startup_timeout_restores_without_hanging`,
  `stalled_fifo_stop_during_startup_restores_without_hanging`, and
  `stalled_fifo_stop_after_playing_restores_without_hanging` use the production
  open, decoder and session worker. The daemon fixture consumes one 4096-byte
  chunk, then retains its FIFO reader without consuming any further PCM. Tests
  observe queued PCM, verify consumed bytes stay at 4096, and exercise startup
  timeout, Stop during startup and Stop after confirmed Playing. Worker joining
  is bounded independently of the activation timeout.

The shared startup scenarios retain output restoration, initial volume 42,
media-route release, instance-lock release, takeover-record cleanup, terminal
worker cache, event ordering and exactly-once natural completion assertions.
Event generations are also checked. Existing initial/live-volume 42/73,
recovery-custody, cancellation and natural-EOF tests remain intact.

## Validation scope

Exact-head command results are recorded on the source bead at submission.
These are real local FIFO/GStreamer tests with a contract fixture, not physical
receiver certification. Wider U5 authority/runtime review and physical playback
remain separate; AD1 correction does not claim overall feature approval.
