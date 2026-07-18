# Playback-history contract

Status: foundation implemented on 2026-07-18; production event persistence and smart-playlist
consumers remain the next two P1.3 slices.

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
- One `PlaybackHistoryProgress` value belongs to one queue occurrence, not merely one track ID or
  one output load generation. Duplicate queue entries can therefore count independently.

## Playback occurrences

A new occurrence begins when Tributary successfully adopts:

- an explicitly selected track or a new queue;
- the next or previous queue occurrence;
- an end-of-stream auto-advance; or
- a Repeat One replay after natural end of stream.

Pause/resume, seeking, the three-second Previous restart, buffering, an output retry, and a
same-track output handoff remain the same occurrence. They re-anchor the state rather than creating
a second count opportunity. Replacing the queue, Stop, application restart, or retiring the owning
source ends the old occurrence; partial credit is intentionally in-memory only. A failed or
rejected candidate never becomes a countable occurrence, a terminal error is not natural end, and
a retry generation cannot count separately from the occurrence it is recovering.

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
- Before the next sample after seek, restart, retry, resume, or same-occurrence output handoff, the
  coordinator re-anchors the state without crediting the jump.
- A forward user seek earns no credit and permanently records skip evidence for the
  unknown-duration end-of-stream rule. A backward or equal seek preserves earlier listening
  credit. A retry, resume, or output-handoff re-anchor earns no credit but is not user skip evidence.
- Replaying content after a backward seek may earn new observed listening time, but the occurrence's
  one-shot latch still permits at most one count.
- Progress accounting uses positions and monotonic sequencing, never elapsed UTC time. The UTC
  clock is sampled only when the one-shot threshold decision is emitted—whether by a position
  crossing, late duration acceptance, or qualifying unknown-duration natural end.

"Exactly once" means once per adopted occurrence in the running playback session. Crash-replay
exactly-once semantics would require a durable occurrence-ID ledger and are not claimed here.

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
and supports down/up retry. Rolling it down necessarily discards stored last-played timestamps but
preserves tracks and play counts; reapplying starts those timestamps as null. No history index is
added yet because smart playlists currently materialize the local table before evaluation; an
index would add write cost without serving the current query path.

The persistence slice must atomically saturate `play_count` at `i32::MAX` and set
`last_played_at_ms` to `max(existing, event_timestamp)`. This prevents integer overflow and keeps a
regressed system clock from making the newest recorded play appear older.

## Integration boundary

The foundation slice adds the schema, model conversion, and pure progress state only. It does not
yet write history during playback, refresh the Plays column, or change seeded smart-playlist
results.

The next slice must:

1. own the progress state in `PlaybackSession` separately from output load generations;
2. feed it only generation-accepted duration, position, discontinuity, and natural-end events;
3. latch before dispatching asynchronous persistence so duplicate ticks cannot enqueue duplicates;
4. update one exact local track ID atomically, treating a concurrently deleted row as a no-op;
5. publish a stable-ID history update only after commit; and
6. refresh the local row plus active and cached playlist projections without disturbing queue
   identity.

AirPlay currently needs either periodic accepted position publication or explicit coverage of its
authoritative natural-end path before the cross-output contract is complete.

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
unknown natural end, forward-skip suppression, and the one-shot latch. Integration coverage in the
next slice must additionally prove rejected loads, stale generations, duplicate output ticks,
source/output replacement, deleted rows, count saturation, post-commit UI publication, and every
supported output's progress or natural-end path.
