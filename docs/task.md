# Tributary active implementation backlog

Last audited: 2026-07-19

This is the executable backlog for feature fixes and additions. It replaces the completed
holistic-review tracker, which is preserved as
[`task-remediation-2026-07.md`](task-remediation-2026-07.md). The rationale and broader
product context live in [`roadmap.md`](roadmap.md), while GitHub remains authoritative for issue
state.

## How to use this file

- Work from the highest-priority unchecked item whose dependencies are satisfied.
- A top-level checkbox is one countable implementation record. Check it only after the behavior,
  tests, user documentation, and `CHANGELOG.md` entry are merged.
- Add the implementing PR beside a completed item and record it in the implementation log.
- Split a record before implementation if it cannot be reviewed safely in one PR. Keep the
  numerator and denominator synchronized with the literal top-level checkboxes.
- Scope protocol, schema, authority, cross-output, and privacy decisions in a design document or
  refined GitHub issue before committing to an implementation.
- Do not treat the order below as a release promise. It is a dependency-aware starting order and
  can change as issues receive product decisions and milestones.

Current status: **6/35 (17.1%)** active implementation records complete. This percentage measures
checklist completion, not equal engineering effort: several P3 records are deliberately large
epics. The archived remediation remains **220/223 (98.7%)** complete; its three open records are
real-environment validation, not missing implementation.

## Current focus

P1.1, P1.2, all three P1.3 playback-history records, and the P1.4 rating foundation are complete.
Continue with P1.4's accessible local editing, display, sorting, and smart-playlist rules, including
honest read-only and unsupported source states. The rest of P1 builds on the source-scoped identity already
present in the runtime, the now-bounded shuffle navigation semantics, an honest local-only playlist
interaction boundary, and authoritative local playback history with deterministic live consumers.

The independent Linux watcher correctness fix tracked in
[#103](https://github.com/jm2/tributary/pull/103) does not change the **6/35** feature total.
The salvaged scope rejects explicitly classified access/access-time noise before the bounded watcher
queue without filtering real bootstrap mutations or backend errors, while retaining overflow
evidence for authoritative reconciliation. It intentionally omits the original persistent
unparseable-file cache so transient parser and I/O failures remain retryable. It is tracked outside
the 35-record feature backlog.

## P1 — Correctness and shared feature foundations

### P1.1 — Harden and document existing shuffled playback history

- [x] Bound, specify, and fully regress the existing occurrence-aware shuffle history
  ([#132](https://github.com/jm2/tributary/pull/132)).

  Acceptance criteria:

  - Previous inside the restart threshold selects the actual prior queue occurrence, not a newly
    randomized track. More than three seconds into a track, the first Previous still restarts the
    current track; a subsequent Previous walks history.
  - Retain the current occurrence plus the ten real prior occurrences. Backward and forward
    traversal move within that fixed history budget; repeat-all must not grow memory without a
    bound, and reaching the retained boundary must not fabricate a random predecessor.
  - Next after one or more Previous operations first walks the corresponding forward history in
    order before drawing an unvisited random occurrence.
  - Duplicate tracks remain distinct queue occurrences. Repeat off/all/one, cycle rollover, one-
    and two-item queues, failed-load rollback, and shuffle on/off transitions are explicit.
  - Manual track starts, a new queue, Stop, real output replacement, and owning-source retirement
    reset history. Sort/filter/sidebar navigation, metadata refresh, Pause, and same-output
    reselection preserve it.
  - Tests cover multi-step backward/forward traversal in `PlaybackSession` and pin both the header
    button and operating-system media-control dispatch to that path.

  Implemented contract: `PlaybackSession` retains a chronological timeline capped at the current
  queue occurrence plus ten real predecessors, with an explicit cursor separate from the active
  cycle's randomized bag. Previous never fabricates or wraps at the retained boundary; Next first
  replays fixed forward history. Repeat All rolls into complete occurrence-permutation cycles
  without an immediate boundary repeat, while Repeat One remains an end-of-stream policy. Either
  shuffle-button transition starts a fresh traversal at the unchanged current item. The header
  button and OS media action now share the exact `> 3000 ms` restart dispatcher, and regressions
  cover duplicates, one/two-item queues, rollback, resets, and preservation boundaries.

### P1.2 — Make unsupported remote playlist actions honest

- [x] When Add to Playlist cannot accept a remote row, show a localized, user-visible result
  instead of only logging that the row was skipped ([#47](https://github.com/jm2/tributary/issues/47);
  [#133](https://github.com/jm2/tributary/pull/133)).

  Keep this slice migration-free. It closes the misleading current interaction while P1.5 designs
  full remote playlist persistence.

  Implemented contract: the context menu snapshots the active source together with its selection.
  Only the exact built-in `local` view may enter the local-playlist database path. Choosing a
  destination playlist from an authenticated remote, Radio-Browser, removable source, or any
  unknown/pathless view now presents a localized all-or-none explanation before a runtime task or
  database connection is created, so no unsupported selection can be partially written. Existing
  Add/Remove/Properties menu labels now use their shipped translations as well. Policy regressions
  fail closed across exact remote/radio/removable identities and malformed keys, and all 13 locale
  catalogs carry non-fallback result copy. Persistent remote playlist entries remain P1.5.

### P1.3 — Record trustworthy local playback history

- [x] Define and migrate the durable playback-history contract: counted-play threshold,
  `last_played`, repeat/seek/restart semantics, clock representation, and legacy-row behavior
  ([contract](playback-history.md); [#134](https://github.com/jm2/tributary/pull/134)).
- [x] Persist play-count and last-played updates from authoritative playback events exactly once,
  without counting rejected loads, stale generations, or retries, and refresh affected UI state
  ([#135](https://github.com/jm2/tributary/pull/135);
  [#136](https://github.com/jm2/tributary/pull/136)).
- [x] Make Recently Played and Top 25 reflect the new history contract deterministically, including
  live refresh, ordering, empty-state, migration, and regression coverage
  ([#137](https://github.com/jm2/tributary/pull/137)).

  Implemented history pipeline: local tracks have a nullable UTC epoch-millisecond `last_played`
  field, negative legacy counts are repaired without inventing timestamps, and `PlaybackSession`
  owns one-shot progress separately from replaceable output-event generations. Only a successfully
  accepted exact local occurrence—including its regular/smart-playlist projection—can earn
  observed forward-playback credit. Rejected and stale deliveries earn none; retries, pause,
  buffering, seeks, and the three-second Previous restart re-anchor the same occurrence without
  jump credit. Paused/Stopped position polls stay inert until Playing; navigation and Repeat One
  create fresh occurrences, while the current output-target replacement ends playback.

  Once an occurrence qualifies, its latch closes before synchronous FIFO enqueue. The library
  engine atomically updates only that stable `TrackId`, repairs a legacy-negative count to the
  first legitimate play, saturates `play_count` at `i32::MAX`, keeps the greatest
  existing/event timestamp, and treats a concurrently deleted row as a clean no-op. Repeat One
  rolls back only its tentative history occurrence on a pre-generation failure and never clones or
  restores the wider playback session.
  Normal shutdown first closes one shared GTK command-admission gate, disables every playback,
  media-key, seek, open-file, history, and root-trust producer, appends a FIFO marker, revokes event
  ownership, stops playback, and waits for all earlier admitted history/root-trust commands.
  Nothing can be admitted behind that marker while an initial or root-trust scan delays it; this
  barrier deliberately does not claim to drain filesystem-watcher events, and the disabled window
  may remain visible until the serialized scan finishes. Only a committed update publishes the
  replacement row; the live local Plays value
  refreshes by stable ID and active/cached playlist projections are invalidated. AirPlay 1 now
  contributes the same evidence through generation-scoped 500 ms position samples. The complete
  contract is in [`playback-history.md`](playback-history.md).

  The seeded history consumers now use one immutable clock snapshot per evaluation. Recently
  Played accepts only representable, non-future `last_played` instants in the inclusive preceding
  14 days, orders newest first, and uses stable `TrackId` as its final tie-breaker; null, corrupt,
  legacy-unknown, and out-of-window timestamps are excluded, so all-unknown history produces an
  intentional empty result instead of a match-all fallback. Top 25 accepts only positive play
  counts, selects and presents at most 25 by descending count, then descending last-played with
  unknown timestamps last, then stable `TrackId`. Legacy positive counts remain eligible even when
  their timestamp is null.

  A committed history event invalidates every cached playlist projection, rejects older
  asynchronous results by navigation generation, and immediately reloads an active playlist, so
  membership and order update without a restart. Fresh databases persist the canonical rules.
  Migration 11 atomically rewrites both matching defaults in one transaction, recognizing only the
  exact untouched Tributary defaults represented by the released v0.5.0 JSON with
  `live_updating: true` or its immediate no-field successor. The English name, smart flag,
  byte-exact rules, and redundant match/live/limit columns must all match; renamed, edited,
  reformatted, non-smart, non-live, or otherwise divergent playlists remain byte-for-byte
  user-owned. Only the rules JSON changes; IDs, timestamps, and compatibility columns are
  preserved, and rollback/retry is safe.

  The smart-playlist editor now exposes Last Played filtering and sorting plus Most/Least Recently
  Played limit selection. Reopening and saving a relative-date rule preserves its amount and its
  authorable Days, Weeks, or Months unit. The complete implemented contract and validation matrix
  are in [`playback-history.md`](playback-history.md).

### P1.4 — Add ratings as a real library field

- [x] Decide rating ownership and capability semantics, then add the database migration, model,
  backend propagation, import/export representation, and safe legacy defaults
  ([#37](https://github.com/jm2/tributary/issues/37),
  [#138](https://github.com/jm2/tributary/pull/138)).
- [ ] Add accessible editing, display, sorting, and smart-playlist rules, with explicit behavior for
  read-only or rating-incapable sources.

  Implemented foundation: [`ratings.md`](ratings.md) defines a canonical whole integer from 1
  through 100, with `None` as unrated, and one coherent Writable, ReadOnly, or Unsupported state per
  track. Tributary owns and transactionally writes ratings only for exact local-library IDs in
  SQLite; migration 12 leaves every legacy row `NULL`, enforces integer/range storage, validates
  interrupted upgrades, and supports down/up retry. Existing-row metadata refreshes and recognized
  paired watcher file/directory renames preserve ratings; an offline or otherwise unrecognized
  remove-plus-add becomes a new unrated row. Embedded tags are neither read nor written for this
  field.

  Subsonic's valid integer 1–5 `userRating` maps read-only to 20-point increments. Jellyfin and Plex
  accept only finite native user ratings in 0–10, round the tenfold value, and preserve native zero
  as canonical 1; malformed or absent values are read-only unrated. DAAP, radio, removable,
  external, and unknown sources are Unsupported, all remote mutations fail closed, and catalogue
  admission rejects per-track/source capability disagreement. XSPF v1 intentionally emits no
  rating and ignores rating-like metadata on import; playlist matching never mutates library
  ratings. A future metadata transfer requires separate opt-in conflict handling.

### P1.5 — Persist source-scoped playlists

- [ ] Design and migrate regular playlist entries from local track foreign keys to stable
  source-scoped `(SourceId, TrackId)` identity, with deterministic local migration, ordering,
  duplicate-occurrence, unavailable-source, deletion, and rollback behavior
  ([#47](https://github.com/jm2/tributary/issues/47)).
- [ ] Resolve remote playlist entries only through the live `SourceRegistry` session; implement
  Add/Remove/Play UI, disconnected states, source retirement, stale-epoch rejection, and mixed-source
  playlist tests without persisting locators or credentials.
- [ ] Treat Subsonic server-native playlist import/synchronization as a separate capability slice,
  with direction, conflict, offline, deletion, and server-feature semantics documented first.

## P2 — User-facing integrations and bounded enhancements

### P2.1 — Migration and listening integrations

- [ ] Import Rhythmbox `rhythmdb.xml`, playlists, play counts, and ratings transactionally and
  idempotently, with exact non-guessing matching and actionable conflict/unmatched reporting
  ([#57](https://github.com/jm2/tributary/issues/57)).
- [ ] Implement Last.fm authorization and protected secret storage, now-playing/scrobble thresholds,
  durable retry/offline behavior, privacy UX, and source-aware metadata on authoritative playback
  events ([#50](https://github.com/jm2/tributary/issues/50)).

### P2.2 — Drag and drop

- [ ] Add accessible multi-selection drag/drop onto local regular playlists, with stable occurrence
  ordering, clear feedback, cancellation, and keyboard-equivalent behavior
  ([#46](https://github.com/jm2/tributary/issues/46)).
- [ ] Design file-manager export, remote-row drops, and device-copy drops as separate authority and
  transfer policies; implement only the variants whose target semantics are available.

### P2.3 — Library browsing and presentation

- [ ] Add root-relative folder browsing with multiple-root disambiguation, lazy navigation,
  unavailable/renamed-root behavior, and an explicit omission policy for pathless sources
  ([#14](https://github.com/jm2/tributary/issues/14)).
- [ ] Add album artwork to the browser using a virtualized, accessible UI and bounded asynchronous
  loading/cache with cancellation, placeholders, authenticated-art resolution, and persisted layout
  preferences ([#39](https://github.com/jm2/tributary/issues/39)).
- [ ] Re-evaluate and implement the independently useful separator, count-opacity, and alignment
  refinements against current GNOME HIG/theme behavior, with visual and accessibility review
  ([#29](https://github.com/jm2/tributary/issues/29)).

### P2.4 — Audio processing and output protocols

- [ ] Design the equalizer filter graph, band/preset/preamp/clipping contract, live-reconfiguration
  boundary, persistence, and capability matrix for local, AirPlay, Chromecast, and MPD outputs
  ([#49](https://github.com/jm2/tributary/issues/49)).
- [ ] Implement the supported equalizer path and accessible settings UI, then test format changes,
  gapless navigation, disabled/bypass behavior, clipping policy, and each output's supported or
  explicitly unavailable state.
- [ ] Open and complete an AirPlay 2 sender design investigation covering maintained dependencies,
  pairing, encrypted control, audio/timing, licensing, packaging, and real-device test requirements.
- [ ] Implement the selected AirPlay 2 pairing/control/audio path without exposing unsupported
  discovered receivers in the output selector; keep simultaneous multi-room sync out of scope
  unless separately approved.
- [ ] Validate AirPlay 2 interoperability and packaging on supported platforms and representative
  receivers, including reconnect, cancellation, authentication failure, and actionable diagnostics.
- [ ] Add receiver-facing IPv6 Chromecast media tickets where a reachable scoped address can be
  published safely; retain fail-closed omission for unusable endpoints.
- [ ] Design and implement an optional detectable MPD exclusive-control/ownership mode before
  enabling automatic orphan cleanup; account for partition-global playback and option commands.

## P3 — Large data-movement epics and engineering follow-ups

### P3.1 — Offline remote media

- [ ] Design persistent source-scoped offline identity, authenticated/resumable download jobs,
  atomic storage, server capability, credential, licensing, and reconciliation contracts
  ([#11](https://github.com/jm2/tributary/issues/11)).
- [ ] Implement the bounded download/cache engine with restart recovery, integrity checks,
  cancellation, quota/eviction, source replacement, and offline catalogue resolution.
- [ ] Add accessible download/progress/storage UI and test online-to-offline transitions, stale
  servers, partial files, quota pressure, logout, and cache deletion.

### P3.2 — Android and device synchronization

- [ ] Build a generic mounted-filesystem transfer planner/executor with retained write authority,
  capacity/conflict policy, atomic copy where possible, progress, cancellation, and rollback
  ([#8](https://github.com/jm2/tributary/issues/8)).
- [ ] Add MTP discovery and bounded browsing/transfer for typical Android devices without treating
  host paths as portable device identity.
- [ ] Add playlist mapping, incremental state, conflict resolution, and explicitly opted-in
  auto-sync with safe attach/detach recovery.

### P3.3 — Authority and queue extensions

- [ ] Add typed retained mutation authority before enabling Properties/tag writes for pathless
  removable rows; revalidate the mount, ancestry, exact file, write rights, and replacement target
  through commit.
- [ ] If product-approved, turn multi-file OS-open deliveries into an occurrence-preserving
  ephemeral queue; keep the current first-valid-file behavior documented until then.

### P3.4 — Maintenance and coverage

- [ ] Re-evaluate `paste`, `proc-macro-error2`, and the inactive lockfile-only RSA advisory by
  2026-10-10 or the next release, and immediately if MySQL support is enabled.
- [ ] Remove the macOS GStreamer channel-cap workaround only after the upstream fix is in the
  supported runtime floor and passes affected multi-channel hardware testing.
- [ ] Add a direct end-to-end watcher-backlog/root-confirmation ordering harness if its incremental
  coverage remains worth the platform-fixture cost.

## Explicitly outside this backlog

- The three unchecked records in the archived remediation tracker are physical/installed/live
  validation: real removable hardware, installed Flatpak portal/USB behavior, and packaged Windows
  DAAP/Subsonic playback. They do not imply another implementation slice unless testing finds a bug.
- The intentionally skipped release-workflow exercise and proper Apple signing/notarization are
  distribution work, not part of this feature percentage.
- Direct Apple/iTunes XML, Google Takeout CSV, M3U, service-URL playlist input, and fuzzy
  “similarly named” matching are not scheduled. XSPF is the supported interchange path.
- Automount/eject, markerless read-only root enrollment, stronger native removable IDs, and saved
  endpoint rebind are candidates rather than committed work until they receive scoped issues.

## Implementation log

| Date | Task | PR | Result |
|---|---|---|---|
| 2026-07-18 | Backlog reset | — | Archived the holistic-review tracker and established the audited feature backlog; no implementation record completed. |
| 2026-07-18 | P1.1 bounded shuffle history | [#132](https://github.com/jm2/tributary/pull/132) | Retained ten real prior occurrences, fixed forward traversal and complete Repeat All cycles, unified Previous dispatch, made toggle/reset semantics explicit, and added lifecycle/rollback regressions. |
| 2026-07-18 | P1.2 honest unsupported playlist actions | [#133](https://github.com/jm2/tributary/pull/133) | Refused non-local Add to Playlist actions with an all-or-none localized dialog before database work, localized the existing context-menu labels, and regressed the fail-closed source policy plus every shipped catalog. |
| 2026-07-18 | P1.3 playback-history contract and schema | [#134](https://github.com/jm2/tributary/pull/134) | Defined occurrence, threshold, duration, seek/retry/restart, clock, and legacy contracts; added migration 10 plus safe model conversion and a pure one-shot progress state. Production event writes and smart-playlist consumers were tracked as follow-on records. |
| 2026-07-18 | P1.3 authoritative playback-history persistence | [#135](https://github.com/jm2/tributary/pull/135), [#136](https://github.com/jm2/tributary/pull/136) | Bound one progress latch to each exact local queue occurrence independently of output generations; rejected/stale/retry events cannot double count, paused polls stay inert, Repeat One rolls back only its tentative occurrence before generation handoff, and other discontinuities re-anchor. Added a shared shutdown admission gate plus FIFO drain, atomic stable-ID count/timestamp persistence with legacy-negative repair, post-commit Plays refresh and playlist-projection invalidation, plus generation-scoped AirPlay position evidence. Seeded history consumers were completed in the following record. |
| 2026-07-18 | P1.3 deterministic history smart playlists | [#137](https://github.com/jm2/tributary/pull/137) | Made Recently Played and Top 25 deterministic over authoritative history, including intentional empty states, stable ordering and Top 25 membership, committed-event live refresh, exact untouched-default migration from both released historical signatures, and lossless editor round trips for Last Played fields, limits, and relative units. This completed P1.3. |
| 2026-07-19 | P1.4 rating ownership and persistence foundation | [#138](https://github.com/jm2/tributary/pull/138) | Defined the canonical 1–100 value and coherent Writable/ReadOnly/Unsupported capabilities; added constrained nullable migration 12, transactional exact-local-ID set/clear, scan preservation, validated read-only Subsonic/Jellyfin/Plex conversion, fail-closed catalogue invariants and remote writes, and an explicit rating-neutral XSPF/import policy. Accessible editing, display, sorting, and smart rules remain the next P1.4 record. |
| 2026-07-18 | Linux watcher feedback-loop fix | [#103](https://github.com/jm2/tributary/pull/103) | Narrowed the external proposal to filter self-generated access events before queue admission without filtering genuine startup events or backend errors; bounded overflow still drives authoritative reconciliation. Persistent negative parse caching is deliberately excluded so failures remain retryable; this separate correctness fix does not advance the feature numerator. |
