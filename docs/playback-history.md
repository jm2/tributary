# Playback-history contract

Status: contract/schema foundation and authoritative production persistence implemented on
2026-07-18; deterministic smart-playlist consumers remain the final P1.3 slice.

This document defines when Tributary may record a local play and how that history is represented.
The contract is deliberately stricter than "a load was requested": rejected media, stale output
events, retries, seeks, and wall-clock changes must not manufacture listening history.

## Scope and ownership

- Durable history belongs only to the exact built-in local library. A track played through a local
  regular or smart playlist updates the underlying local track by its stable `TrackId`.
- Authenticated remotes, Radio-Browser, removable media, and ephemeral operating-system-opened
  files do not write into the local `tracks` table. Any source-owned remote count remains remote
  metadata and remote `last_played` remains unknown to this schema. Source-specific history or
  synchronization requires a separate ownership and privacy decision.
- `PlaybackSession` owns one `PlaybackHistoryProgress` value per queue occurrence, independently
  of replaceable output-event generations. A retry may bind a new accepted delivery to the same
  occurrence without opening another count opportunity; duplicate queue entries are distinct
  occurrences and can count independently.

## Playback occurrences

A new occurrence begins when Tributary successfully adopts:

- an explicitly selected track or a new queue;
- the next or previous queue occurrence;
- an end-of-stream auto-advance; or
- a Repeat One replay after natural end of stream.

The occurrence cannot earn credit until its current output load is successfully accepted. Pause
or resume, seeking, the three-second Previous restart, buffering, and an output retry remain the
same occurrence. They re-anchor the state rather than creating a second count opportunity. A
future same-track delivery handoff that preserves the session must do the same; Tributary's current
output-target replacement instead clears playback and ends the occurrence. Replacing the queue,
Stop, application restart, or retiring the owning source also ends the old occurrence; partial
credit is intentionally in-memory only. A failed or rejected candidate never becomes a countable
occurrence, a terminal error is not natural end, and a retry generation cannot count separately
from the occurrence it is recovering.

## Counted-play threshold

For a positive known duration, one occurrence counts after this much observed forward playback:

```text
min(ceil(duration_ms / 2), 240_000 ms)
```

The first positive duration is authoritative for that occurrence and freezes the threshold. The
library duration may supply it at occurrence creation; otherwise the first positive duration from
the accepted output may supply it later. Zero means unknown, and later disagreement cannot move a
frozen threshold. If already-observed credit meets the newly lowered threshold, accepting that
duration emits the occurrence's one count decision immediately.

An unknown-duration occurrence counts after 240,000 milliseconds of observed forward playback. It
may instead count at authoritative natural end of stream only when no forward user seek supplied
evidence that unobserved content was skipped. Natural end does not grant an unobserved tail to
known-duration media.

## Creditable evidence

- Only position events already accepted for the occurrence's current output generation can earn
  credit. A strictly advancing sample contributes its delta from the previous anchor.
- Pause and buffering time earn nothing. Duplicate or unexpectedly regressed samples earn nothing.
  The first accepted position may prove playback after initial load, retry, or Buffering and is
  used only as a no-credit anchor. After an explicit Paused or Stopped event, position polling
  cannot restore credit until an explicit Playing event; this keeps MPD's paused polls inert.
- Before the next sample after seek, restart, retry, resume, or a future same-occurrence delivery
  handoff, the coordinator re-anchors the state without crediting the jump.
- A forward user seek earns no credit and permanently records skip evidence for the
  unknown-duration end-of-stream rule. A backward or equal seek preserves earlier listening
  credit. A retry, resume, or delivery-handoff re-anchor earns no credit but is not user skip
  evidence.
- Replaying content after a backward seek may earn new observed listening time, but the occurrence's
  one-shot latch still permits at most one count.
- Local GStreamer, MPD, Chromecast, and AirPlay 1 feed the shared generation-scoped event contract.
  AirPlay's dedicated RAOP pipeline samples position and duration every 500 ms only while Playing;
  its weak session reference stops the timer at teardown, and a delayed old-generation sample is
  rejected rather than credited to a replacement load.
- Progress accounting uses positions and monotonic sequencing, never elapsed UTC time. The UTC
  clock is sampled only when the one-shot threshold decision is emitted—whether by a position
  crossing, late duration acceptance, or qualifying unknown-duration natural end.

"Exactly once" means once per adopted occurrence in the running playback session. Normal window
shutdown first closes the shared GTK-thread command-admission gate and makes playback, media-key,
seek, open-file, history, and root-trust callbacks inert. In that same synchronous operation it
appends a FIFO flush marker, then revokes player-event ownership, stops the output, and waits until
every earlier admitted history/root-trust command has finished. Because all UI producers share the
closed gate, no callback can append work behind the marker while a long initial or root-trust scan
delays its acknowledgment. This command barrier does not claim to drain filesystem-watcher events,
and the disabled window can remain visible while earlier serialized scan work finishes. A process
crash or forced termination can still lose in-memory partial credit or an uncommitted decision;
crash-replay exactly-once semantics would require a durable occurrence-ID ledger and are not
claimed here.

## Durable representation and migration

Local `tracks` rows retain the existing non-null signed `play_count` and gain:

```text
last_played_at_ms BIGINT NULL
```

`last_played_at_ms` is a UTC Unix timestamp in milliseconds captured at the counted-play decision.
`NULL` means that Tributary has no trustworthy counted-play instant. The architecture model exposes
it as `Option<DateTime<Utc>>`; an out-of-range stored timestamp safely maps to `None`, and older
serialized tracks that omit the field deserialize as unknown.

Migration 10 has intentionally conservative legacy behavior:

- preserve every zero or positive play count;
- normalize a corrupt negative play count to zero;
- leave `last_played_at_ms` null for every existing row, including rows with a positive count; and
- never infer listening history from date-added, date-modified, file mtime, or playlist dates.

The migration validates an already-present column before completing an interrupted SQLite upgrade
and supports down/up retry. Fresh schema uses the declared `BIGINT` spelling; validation also accepts
SQLite's equivalent nullable, default-free `INTEGER` spelling while rejecting incompatible type,
nullability, default, or primary-key shapes. Rolling the migration down necessarily discards stored
last-played timestamps but preserves tracks and play counts; reapplying starts those timestamps as
null. No history index is added yet because smart playlists currently materialize the local table
before evaluation; an index would add write cost without serving the current query path.

Production persistence atomically saturates `play_count` at `i32::MAX` and sets
`last_played_at_ms` to `max(existing, event_timestamp)`. The bound update targets one exact stable
local `TrackId`, preventing both integer overflow and a regressed system clock from making the
newest recorded play appear older. A row deleted before the transaction is a clean no-op: Tributary
does not append a replacement or match by URI. The updated row is selected inside the same
transaction and becomes a library event only after the commit succeeds.

## Production integration boundary

The production pipeline now implements the schema and progress contract end to end:

1. `PlaybackSession` owns occurrence progress separately from output load generations and feeds it
   only accepted duration, position, discontinuity, and natural-end events.
2. Rejected or stale deliveries cannot earn credit. Retry, resume, pause, buffering, and explicit
   seek/restart re-anchor rather than manufacture a delta; successful navigation and Repeat One
   start fresh occurrences. Explicit Paused/Stopped state cannot be overturned by paused position
   polls without a later Playing event.
3. The one-shot decision latches before synchronously entering the library engine's FIFO, so
   duplicate output ticks cannot enqueue duplicate writes for one occurrence. Normal shutdown
   atomically closes the shared UI admission gate before appending a terminal FIFO marker, makes
   every playback/history/root-trust producer inert, and waits for the marker before closing.
4. The library engine updates one exact local stable ID in a transaction, treats a missing row as
   a no-op, and publishes the selected replacement row only after commit.
5. The UI replaces the exact existing local row by stable ID, allowing a Plays-sorted view to
   reorder immediately, and invalidates active and cached regular/smart-playlist projections. It
   never URI-matches or appends a row that disappeared concurrently.
6. AirPlay 1 now publishes the same accepted position evidence as the other implemented outputs
   through a generation-scoped 500 ms timer.

This pipeline records exact local history but intentionally does not yet change the seeded
Recently Played or Top 25 rules. Their deterministic filtering, ordering, empty-state, live refresh,
and safe untouched-default migration remain the final P1.3 slice.

## Smart-playlist consumers

The final P1.3 slice will make the seeded consumers deterministic:

- **Recently Played:** tracks with a non-null `last_played` in the preceding 14 days, newest first,
  with stable track identity as the final tie-breaker.
- **Top 25 Most Played:** tracks with `play_count > 0`, selected by descending play count, then
  descending last-played time, then stable track identity.

Legacy positive counts remain eligible for Top 25. A legacy null `last_played` never qualifies for
Recently Played until a real counted occurrence supplies one. Empty history produces intentional
empty playlists rather than a match-all fallback. Existing seeded playlists may be migrated only
when their smart flag and exact serialized historical rules still match Tributary's old defaults;
renamed or edited user playlists must be preserved.

## Required validation

The progress component pins half-duration rounding, the four-minute cap, missing and zero duration,
late duration discovery, exact threshold edges, duplicate and regressed samples, pause/retry
re-anchoring, forward and backward seeks, restart, Repeat One occurrence separation, known and
unknown natural end, forward-skip suppression, and the one-shot latch. Production integration
regressions additionally cover local-only and playlist-projected identity, rejected loads, stale
generations, delivery retry, discontinuity re-anchoring, duplicate occurrences, navigation and
repeat boundaries, exact-ID atomic updates, deleted rows, negative-count repair, count saturation,
regressed timestamps, committed-only publication, stable-ID row replacement, playlist-projection
invalidation, and generation-preserving AirPlay position samples. The final slice must add the
consumer and seeded-default migration regressions described above.
