# AL1 / AL2 corrective mapping

Source: `tr-t3a`; review: `refinery-20260916-bcd542ca-tr-t3a`.

- **AL1 — verified endpoint authority:** `OwnToneClient::new` disables automatic
  proxies and redirects on the dedicated blocking client. The existing request
  timeout is retained. No redirected hop can receive control requests or bodies.
- **AL2 — confirmed HTTP success:** all three transport methods (`get_json`,
  `put`, `put_json`) require `status().is_success()` before consuming JSON or
  reporting a mutation as complete. Non-2xx replies enter the existing failure
  and unsettled-mutation recovery paths; a refused restore cannot remove the
  takeover record.

`control_transport_stays_on_verified_daemon` runs in an isolated child with
uppercase and lowercase HTTP/ALL proxy variables pointing to a foreign listener
and empty NO_PROXY. The parent proves that listener receives zero connections.
The child first exercises valid controller playback (200 JSON, 204 mutations),
then thirty owned recording-daemon controller scenarios: 302, 307, and 308,
with and without a foreign Location, on config observation, takeover output
selection/queue clearing, and live stop/output restoration. The fixture refuses
these requests without applying their mutations, including across daemon
restarts. Observation status rejection is also asserted directly with valid JSON.

Failed opens assert generation-correct Error/Stopped without Playing/TrackEnded.
Mutating failures preserve the record, instance lock, prepared route, and recovery
custody through an unsuccessful real recovery attempt. Once valid responses
return, recovery restores the exact prior outputs and releases custody, route,
record, and lock. Every scenario proves subsequent usable controller playback.
The capture and environment changes are confined to the child process, avoiding
interference with concurrently running tests. Existing assertions are preserved.

Validation results and exact pushed head are recorded in source/workflow notes.
The unrelated intermittent failure tracked by `tr-9utsm` remains open and is not
claimed fixed. Physical receiver playback and wider implementation approval are
outside this correction.
