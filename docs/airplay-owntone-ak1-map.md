# OwnTone AK1 corrective mapping

The refinery rejected `988cb86757564608f898c58393b38b2edf766e19` because
malformed daemon observations could authorize takeover and discard the prior
enabled-output set.

## Correction

Both player-state readers use the same strict decoder: only the correctly typed
states `play`, `pause`, and `stop` are accepted. The intentional takeover policy
remains unchanged: playing refuses takeover; stopped and paused permit it.
Malformed progress observations now fail and mark the cached observation stale,
rather than presenting an unknown state as stopped.

Output snapshots require every entry to contain the API's decimal string ID
and a boolean selection. Missing, wrongly typed, or unparseable IDs, missing or
wrongly typed selection, and duplicate IDs reject the entire snapshot. Optional
display names still do not carry authority. No entry is silently discarded or
assumed disabled. These reads still execute on the existing bounded worker path,
before writing a takeover record or sending any mutating RPC.

## Production-controller regressions

Three parameterized tests run fifteen owned recording-daemon scenarios through
the production client, open path, controller, protected local-media route, and
actual WAV/FIFO playback:

- Player responses: missing, null, numeric, and unknown state. Both state and
  progress readers reject each malformed response.
- A valid target plus a prior output with missing, unparseable, null, or numeric
  ID; missing, string, or null selection; and a duplicate target ID.
- Recognized stopped and paused states allow playback; playing refuses takeover.

Every rejected load requires a generation-correct Error and Stopped outcome,
no Playing/TrackEnded, zero mutating requests, no takeover record, released
advisory lock and route, no recovery custody, and unchanged output selection,
player state and queue. No UI Stop causes failure settlement. Every scenario
then loads successfully on the same daemon/controller with a new generation,
plays, stops, releases its route, and restores the exact prior selected outputs.
Accepted policy cases also check restoration before their subsequent load.
Controller load remains nonblocking.

Existing AJ1/AI1/AH1 tests and assertions are unchanged. The unexplained intermittent
timeout recorded in `tr-9utsm` remains open; this correction does not claim to
resolve it. Physical playback and broader implementation approval remain outside
this correction. Exact-head validation is recorded on the source bead at handoff.
