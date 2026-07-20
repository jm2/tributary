# Tributary implementation roadmap

Last audited: 2026-07-20

This document explains the product and engineering work that remains **after** the holistic-review
remediation. [`task.md`](task.md) is the countable active implementation backlog; the completed
remediation record is preserved separately in
[`task-remediation-2026-07.md`](task-remediation-2026-07.md) at **220/223 (98.7%)**, with only three
real-environment validation records left. The feature backlog is now **13/38 (34.2%)** complete.
Neither percentage estimates equal engineering effort, and the historical percentage is not a
claim that Tributary has implemented every requested product feature.

The entries below are candidates, not release promises. As of this audit, 10 GitHub issues remain
open. Server-playlist issue [#143](https://github.com/jm2/tributary/issues/143) has completed
implementation in the current PR pending creation and is expected to close from that PR body on
merge; it is retained in the live table until GitHub records that transition. Other candidates
should receive acceptance criteria, dependencies, and a milestone before work starts. Historical
holistic-review documents are point-in-time findings, not active roadmaps.

## Current baseline

- The P0-P3 implementation remediation is complete. The remaining in-scope tracker records require
  physical removable hardware, an installed interactive Flatpak environment, or packaged Windows
  playback against live DAAP and Subsonic servers.
- Local, Subsonic, Jellyfin, Plex, and DAAP publish complete catalogues through the shared
  `MediaBackend` seam. Connected remotes, Radio-Browser, removable media, and operating-system-opened
  files use the common `SourceRegistry` lifecycle and playback-time authority model.
- AirPlay 1/RAOP, Chromecast, MPD, and local playback are implemented. AirPlay 2/HomeKit is not.
- Regular-playlist storage now has a source-scoped foundation: migration 13 gives every valid
  existing entry the built-in local `SourceId`, makes `(source_id, track_id)` canonical, and
  retains a separate nullable local-track foreign-key cache for deletion and reconciliation.
  Regular playlists now mix exact local occurrences with current authenticated Subsonic,
  Jellyfin, Plex, and DAAP entries. Add first resolves a complete selection through the default-deny
  live registry, then revalidates after staging SQL and retains exact authority permits through its
  atomic commit or rollback; Remove uses exact durable occurrence IDs, so
  duplicates remain independent. Rendering preserves every position and shows disconnected,
  retired/unavailable-source, unsupported-source, invalid-catalogue, missing-track, or
  missing/unmatched-local entries as localized unavailable rows that stay removable. Stale
  projected work/results are discarded and the playlist is invalidated and reprojected. It never
  displays a persisted fingerprint as stale metadata or uses one to guess a remote replacement.
  Each queue item keeps its real source owner;
  remote stream and artwork access revalidates exact epoch, accepted catalogue generation,
  membership, and capability at use while exposing no locator, credential, lease, route, or raw
  backend failure. Only local occurrences own local playback history, and remote ratings remain
  read-only or unsupported. Radio-Browser, removable, external, and unknown sources remain
  unsupported. Smart playlists and XSPF import/export remain local-only; mixed-source metadata
  export remains separate. Subsonic server-native integration now has a pull-only
  [accepted contract](subsonic-playlist-sync.md), bounded `getPlaylists`/`getPlaylist` protocol
  support, a default-deny exact-session registry boundary, strict migration-14 link persistence,
  and an atomic Import Copy/Keep Synced manager. Existing mirrors use pre-network revision tickets,
  exact-session commit permits, frozen membership digests, separate conflict/missing state, and
  read-only mutation gates to protect pulls without persisting live authority. A GTK-free
  source/remote/local coordinator now serializes same-key intent through admitted settlement,
  schedules one exact-observed-session reconnect sweep per accepted epoch, indexes exact
  presence/absence, bounds fan-out to eight local operations, joins coordinator and source
  authority only after SQL staging, reports redacted manual completions, and drains admitted work
  during shutdown. The completed final UI adds a capability-filtered virtualized browser with an
  independent latest-only listing lane, bounded display hints, and revocable opaque one-shot
  Import Copy/Keep Synced tokens; native playlist identity and exact selections remain
  Tokio-owned. Accepted browser actions share the exact remote coordinator lane and acquire
  registry authority only after manager SQL staging. Selecting a linked mirror now exposes the
  localized Sync Now, Retry, Replace Local with Server, Unlink, and Remove Local Copy shell from
  typed durable state plus a content-redacted in-memory availability inspection. Network actions
  fail closed without exact current pull authority, while source-independent unlink/removal remain
  available offline. Selection, lifecycle, sidebar, and operation generations reject stale work;
  destructive actions require confirmation, and recycled/running/hidden controls preserve safe
  focus and accessibility state. See the regular-playlist
  [storage contract](source-scoped-playlists.md).
- XSPF v1 import/export is implemented with exact path and deterministic normalized-metadata
  matching. Apple/iTunes XML, Google Takeout CSV, M3U, service URLs, and fuzzy matching are not
  direct input modes. Ratings are intentionally omitted on export and rating-like extension data
  is inert on import because playlist interchange has no metadata-write consent or conflict flow.
- Mounted removable filesystems can be browsed and played. Copy/sync, MTP-only devices, automount,
  eject, and pathless removable tag mutation are not implemented.
- Shuffled playback retains the current queue occurrence plus ten real predecessors. Previous and
  subsequent forward navigation follow that fixed history, Repeat All uses complete bounded
  occurrence cycles, and the header and OS media controls share one exact restart threshold.
- Local tracks have a migrated nullable UTC-millisecond `last_played` field and authoritative
  per-occurrence production persistence. The [playback-history contract](playback-history.md)
  defines occurrence, threshold, duration, seek/retry/restart, clock, and legacy semantics.
  `PlaybackSession` rejects stale/rejected generations and re-anchors discontinuities; the library
  engine atomically updates one stable local track ID, and committed changes refresh the Plays row
  and invalidate playlist projections. AirPlay 1 supplies generation-scoped 500 ms progress.
  Recently Played evaluates one inclusive 14-day clock window over representable, non-future
  timestamps, newest first with stable ID ties. Top 25 selects and presents positive counts by
  count descending, last-played descending with unknown timestamps last, then stable ID, capped at
  25; a legacy positive count with no timestamp remains eligible. Empty or unknown recency history
  does not make Recently Played a match-all playlist. Committed history invalidates cached
  projections and immediately reloads an active playlist behind a navigation-generation guard.
  Fresh installations seed those rules, while migration 11 atomically rewrites only byte-exact
  untouched v0.5.0 defaults and their immediate no-field successors; user-owned variants remain
  unchanged. Last Played editor fields, Most/Least Recently Played limits, and Days/Weeks/Months
  relative units round-trip losslessly.
- The complete [rating contract](ratings.md) defines one canonical whole integer from 1 through 100
  with `None` as unrated. Tributary owns nullable ratings only for local SQLite tracks; migration 12
  leaves legacy rows unrated, metadata refresh preserves values, and exact-ID set/clear is
  serialized, transactional, committed-only, and live in local or playlist views. The visible
  Rating column supports keyboard editing for writable local rows and explicitly labels read-only
  values/unrated state or unsupported rows; Radio-Browser keeps its compact station-only column
  set and omits Rating. Subsonic's valid 1–5 `userRating` and Jellyfin/Plex's
  valid finite 0–10 ratings propagate read-only; DAAP, radio, removable, external, and unknown
  sources remain unsupported. Both column and smart-playlist rating sorts keep missing values last
  in either direction with deterministic ties. Smart filters provide validated 1–100 numeric/range
  predicates and capability-aware Is Rated/Is Unrated behavior, plus Highest/Lowest Rated limits.

## Proposed implementation order

This order favors correcting misleading current interactions and building shared foundations
before starting large protocol or transfer subsystems.

### 1. Correct the playlist and playback-history contracts

1. **Completed: make shuffled navigation follow real history.** `PlaybackSession` now retains a
   bounded occurrence timeline, walks backward and forward through real selections, stops at the
   retained boundary, and starts complete Repeat All cycles without an immediate repeat. Shuffle
   toggles, rollback, lifecycle resets, duplicate occurrences, small queues, and the shared
   header/OS Previous dispatcher are covered by regressions.
2. **Completed: make unsupported remote-to-playlist behavior explicit ([#47]).** The initial slice
   made every then-unsupported non-local selection fail visibly and all-or-none before database
   work. Source-scoped storage, default-deny authority, and the capability-gated mixed-source
   integration now admit current authenticated Subsonic, Jellyfin, Plex, and DAAP rows while
   retaining that refusal for radio, removable, external, unknown, or unavailable selections and
   discarding stale selection results.
3. **Completed: implement trustworthy local playback history.** The durable schema,
   [counted-play contract](playback-history.md), production persistence pipeline, and seeded
   consumers are complete.
   One `PlaybackSession` progress latch follows each exact local occurrence independently of output
   generations; rejected/stale events cannot contribute, pause/buffering/retry/seek/restart
   discontinuities re-anchor, and paused polls stay inert until Playing. A qualifying play enters a
   FIFO whose shared GTK admission gate closes before shutdown's terminal marker, making playback,
   history, root-trust, media-key, seek, and open-file callbacks inert before the drain. The engine
   atomically updates one stable local ID with a saturating count and monotonic timestamp, then
   refreshes the Plays row and invalidates active/cached playlist
   projections only after commit. AirPlay 1 now publishes generation-scoped position evidence on a
   500 ms timer. Recently Played uses one clock snapshot, an inclusive preceding 14-day window over
   valid non-future timestamps, newest-first presentation, and a stable-ID tie-breaker. Top 25
   admits only positive counts—including legacy counts without timestamps—and selects/presents at
   most 25 by count descending, last-played descending with unknown timestamps last, then stable
   ID. Null, corrupt, future, and out-of-window recency evidence cannot turn empty history into a
   match-all result. A committed history event invalidates every cached playlist projection,
   rejects older asynchronous results by navigation generation, and reloads an active playlist.
   Fresh defaults store the canonical rules; one transactional migration recognizes only the
   byte-exact released v0.5.0 JSON (including `live_updating: true`) and its immediate no-field
   successor when the name, smart flag, rules, and redundant match/live/limit columns all match.
   Renamed, edited, reformatted, or otherwise divergent playlists remain byte-for-byte user-owned.
   The editor exposes Last Played filtering/sorting and Most/Least Recently Played limits while
   preserving relative-rule amounts and Days/Weeks/Months units across reopen/save.
4. **Completed: add ratings ([#37]).** The [rating contract](ratings.md), migration,
   model/backend propagation, transactional exact-local-ID persistence, validated read-only
   Subsonic/Jellyfin/Plex conversion, rating-neutral XSPF boundary, accessible capability-aware
   column/editor, committed live refresh, deterministic null-last sorting, and validated smart
   filters/sorts/limits are complete.
5. **Completed: establish source-scoped regular-playlist storage ([#47], [#140]).** The
   [storage contract](source-scoped-playlists.md) defines one durable occurrence as the exact
   `(SourceId, TrackId)` of its owning source while keeping playlists as `ViewOrigin`s. Migration 13
   deterministically converts every valid existing entry to the local source, preserves entry
   identity, ordering, duplicates, fingerprints, and local `ON DELETE SET NULL` behavior through a
   separate cache, rejects non-local path evidence, and passed exact up/down/rollback, compatibility,
   debug/release, and strict-lint validation.
6. **Completed: publish default-deny live-catalogue playlist authority ([#141]).**
   `ManagedSourceAdapter` exposes an explicit `Unsupported` or `SourceScopedEntries` capability, with
   only authenticated Subsonic, Jellyfin, Plex, and DAAP adapters opting in. Lookup returns one
   ordered resolution per requested occurrence against the exact current source, session epoch,
   and accepted catalogue generation; repeated requested IDs preserve occurrence order. Missing
   exact tracks, unavailable sessions, and unsupported sources return fixed unavailable results
   independently. An otherwise accepted catalogue with a missing or duplicate native identity
   receives an `Invalid` playlist-authority index, so every playlist occurrence for it is
   unavailable without discarding the catalogue from existing non-playlist UI. Accepted metadata
   crosses a dedicated display/sort/rating/history whitelist rather than a `Track` clone; its closed
   guard carries the non-secret epoch and generation transiently, and neither is persisted.
   Stream/artwork resolution rechecks capability, membership, epoch, and generation before and
   after asynchronous adapter work, closes raw failures to fixed categories, and carries an
   explicitly revoked lifecycle generation lease through consumption even if an old snapshot clone
   remains alive. Connecting or failed replacement may
   retain an accepted predecessor; successful replacement, same-session refresh, disconnect,
   shutdown, and final release invalidate or deny old guards at their defined boundaries. This
   internal foundation was not itself an Add/Remove/Play feature; [#142] is its reviewed consumer.
7. **Completed: integrate mixed-source regular-playlist UI ([#142]).** Add consumes Record A's exact
   current authority for authenticated Subsonic, Jellyfin, Plex, and DAAP entries. Its transaction
   revalidates after staging SQL and acquires an exact permit immediately before committing the
   entire ordered selection or nothing. A stale final check rolls back; lifecycle invalidation
   after admission waits for commit or rollback. Remove addresses durable occurrence IDs atomically.
   Projection preserves ordering and duplicates while retaining explicit removable unavailable
   rows without stale metadata or fingerprint matching. Queue items use each occurrence's real
   source, and guarded remote stream/artwork resolution rejects refresh, replacement, retirement,
   disconnect, stale epoch/generation, or missing membership at use. Local history ownership and
   remote rating capability do not widen. Radio-Browser, removable, external-file, and unknown
   sources remain unsupported; smart playlists and XSPF import/export remain local-only, and a
   remote or unresolved regular occurrence makes XSPF export refuse all-or-none rather than emit a
   local-only subset. Native Subsonic persistence/pull and UI were deliberately separate follow-on
   policies: the engine is now complete below, while its user-facing integration and mixed-source
   metadata export remain separate.
8. **Completed foundation: define and read server-native Subsonic playlists ([#143]).** The
   [pull-sync contract](subsonic-playlist-sync.md) separates one-time detached Import Copy from an
   opt-in read-only Keep Synced mirror, forbids server mutation and fuzzy matching, and pins
   conflict, offline, cancellation, server-deletion, unlink, and privacy behavior before schema or
   UI work. Bounded `getPlaylists` and `getPlaylist` reads preserve exact playlist/track IDs, order,
   and duplicate occurrences while rejecting malformed or partial responses all-or-none.
   `ManagedSourceAdapter` defaults this capability to Unsupported; only authenticated Subsonic
   opts into PullSnapshots. List/detail work is accepted only when the exact adapter, session epoch,
   and revocable lease remain current before and after network I/O. The endpoint snapshot neither
   depends on music-catalogue membership nor grants playback authority.
9. **Completed engine: persist and atomically apply Subsonic mirrors.** Migration 14 adds one
   exact, non-secret pull-only link per source/native playlist identity with strict schema
   recognition and non-lossy downgrade refusal. Import Copy is detached; Keep Synced is read-only.
   A separately compared name, frozen ordered-membership digest, orthogonal local-conflict/server-
   presence state, last-success timestamp, and revision CAS preserve the last complete snapshot and
   reject stale durable results. Successful list/detail receipts can acquire an operation-bound,
   session-only permit only after SQL staging; persistence verifies that it was minted for the same
   sealed pull or absence result, so pre-admission staleness rolls back and invalidation after
   admission waits for commit. Pull, conflict, explicit Replace, complete-list missing, Unlink, and
   explicit removal are atomic; ordinary mutation and reconciliation reject linked mirrors.
   Record E structural UI groundwork ([#146](https://github.com/jm2/tributary/pull/146)) now
   publishes typed joined link/sidebar state, keeps mirrors
   out of ordinary mutation affordances, publishes ordinary CRUD only after commit, and reserves a
   separate localized recovery/status shell. A follow-up durable SQLite revision and
   lifecycle-owned full-snapshot publisher now order scan seeding, CRUD, raw/cascade domain-table
   writes, and link-state mutations; GTK rejects equal or older delivery.
10. **Completed lifecycle slice: coordinate and schedule server-playlist operations
    ([#148](https://github.com/jm2/tributary/pull/148)).** Three typed,
   content-redacted lanes cover source-wide discovery, exact remote playlist actions, and durable
   local mirrors. A global logical-request stamp orders reconnect discovery against newer manual
   work, with direct reserve-and-enqueue atomic against delayed stamped fan-out; same-key successors
   wait through admitted task and guard settlement while unrelated keys stay concurrent. Reconnect
   observes exact accepted session epochs, skips server I/O when no mirror is linked, prepares every
   durable ticket before one complete indexed list, and runs at most eight detail/commit operations
   concurrently (measured with a held ninth). Detail failure never becomes deletion evidence.
   Pull/missing persistence joins final coordinator
   admission with exact registry authority after SQL staging, guarded local-only recovery uses the
   same lane, and shutdown closes admission before source revocation and drains admitted work. A
   redacted headless completion facade exists. At [#148](https://github.com/jm2/tributary/pull/148)'s
   merge boundary, the virtualized browser, opaque Import Copy/Keep Synced tokens, and visible
   recovery consumer remained for the following completed slice.
11. **Completed final server-playlist UI and recovery slice (current implementation PR pending
    creation; closes [#143](https://github.com/jm2/tributary/issues/143) on merge).** The Playlists
   header opens a localized virtualized browser containing only sources with exact current
   `PullSnapshots` capability. A separate latest-only list lane cannot cancel reconnect recovery;
   one bounded active session owns opaque action tokens whose reload, lifecycle, close, and
   shutdown revocation never exposes native identity to GTK. Import Copy and Keep Synced consume
   exact tokens only after the eight-action capacity gate, then use the existing remote coordinator
   and post-staging registry authority through commit or rollback. Linked mirrors visibly expose
   Sync/Retry/Replace/Unlink/Remove through targetless actions resolved from the current durable
   local selection, with no-network availability inspection, fail-closed generation gates,
   localized confirmations, consistent sensitivity, and focus-safe state replacement. The layered
   accessibility evidence combines real registry/coordinator/database/sidebar flows,
   deterministic GTK policy tests, complete 13-catalog/40-key parity, and structural review; it
   does not claim a live assistive-technology harness. The pull-only boundary still forbids server
   mutation, periodic polling, fuzzy matching, non-Subsonic authority, and native playlist IDs in
   GTK. Validation passes 92 focused server-playlist tests and locked 1,300-test debug and release
   suites.

These contracts make Rhythmbox migration and Last.fm behavior much less ambiguous.

### 2. Build migration and listening integrations

1. **Rhythmbox migration ([#57]).** Import `rhythmdb.xml`, playlists, play counts, and ratings
   transactionally and idempotently; match files without guessing; and report conflicts and
   unmatched rows. Existing XSPF import is useful but is not automatic Rhythmbox migration.
2. **Last.fm scrobbling ([#50]).** Design account authorization and secret storage, now-playing and
   scrobble thresholds, a durable retry queue, offline behavior, privacy disclosure, and
   source-aware metadata. This should consume the same authoritative playback-history events.

### 3. Add bounded library-management UX

1. **Local playlist and file drag-and-drop ([#46]).** Start with multi-selection drops onto local
   playlists. File-manager export, remote tracks, and device copies have different authority and
   transfer semantics and should be separate follow-ups.
2. **Browse by folder ([#14]).** Define root-aware relative directory identity, multiple-root
   disambiguation, lazy navigation, and rename/unavailable-root behavior. Pathless remote and
   removable sources need an explicit supported-or-omitted policy.
3. **Album art in the browser ([#39]).** Choose an art-enhanced list or virtualized album grid, then
   add bounded asynchronous loading, caching, cancellation, placeholders, authenticated artwork,
   accessibility, and persisted layout preferences.
4. **UI refinements ([#29]).** The requested separator, count-opacity, and alignment changes remain
   open. Confirm each subjective change against the current GNOME HIG and themes before applying
   it, and review the result visually.

### 4. Plan audio-output work explicitly

1. **Equalizer ([#49]).** Define the GStreamer filter graph, bands/presets, preamp and clipping
   policy, live reconfiguration, persistence, and per-output behavior. Local and AirPlay pipelines
   can potentially process locally; Chromecast and MPD may need receiver-side support or an
   explicit unsupported state.
2. **AirPlay 2/HomeKit sender.** Complete a design investigation before choosing a dependency or
   implementation. At minimum this requires pairing, encrypted control, and the expected encoded
   audio/timing path. Multi-device clock synchronization is required only if simultaneous
   multi-room output becomes an explicit goal. See [AirPlay 2](#airplay-2).
3. **Chromecast IPv6 publication.** The current receiver-facing ticket listener is IPv4-only, so an
   IPv6-only Cast control endpoint is omitted rather than given an unreachable media URL.
4. **MPD detectable exclusive-control mode.** Safe automatic orphan cleanup remains coupled to a
   stronger ownership mode that must also account for MPD's partition-global pause, stop, repeat,
   random, single, and consume operations. The current explicit exclusive-control confirmation and
   conservative orphan retention remain the safe behavior until then.

### 5. Treat data movement as design epics

1. **Offline remote cache/download ([#11]).** This needs authenticated resumable downloads,
   source-scoped persistent cache identity, atomic files, quota/eviction, offline catalogues,
   reconciliation, progress/cancellation, and a clear credential and licensing policy.
2. **Android/device synchronization ([#8]).** Mounted-filesystem browsing is only a foundation.
   A real sync feature needs write authority, capacity/conflict planning, incremental state,
   playlist mapping, progress/cancel/rollback, optional auto-sync, and MTP support for typical
   Android devices.
3. **Typed removable mutation authority.** Re-enabling Properties for pathless removable rows
   requires retained, revalidated write authority and safe replacement semantics. Until that
   exists, omitting Properties is intentional and safer than reconstructing a host path.

## Live open issues

This is a snapshot of the remaining open issue set on 2026-07-20. GitHub remains authoritative for
whether an issue is open; this table records the implementation assessment so a feature request is
not mistaken for work already underway.

| Issue | Current implementation state | Likely implementation shape |
|---|---|---|
| [#57 — Rhythmbox playlists, play counts, and ratings](https://github.com/jm2/tributary/issues/57) | No direct importer. XSPF conversion plus completed playback-history and rating contracts are foundations; XSPF deliberately transfers neither history nor ratings. | Build a separate transactional, idempotent migration with explicit metadata consent and conflict reporting. |
| [#50 — Last.fm scrobbling](https://github.com/jm2/tributary/issues/50) | No Last.fm client or scrobble pipeline. | Authorization, secret storage, authoritative thresholds, retry/offline queue, and privacy UX. |
| [#49 — Equalizer](https://github.com/jm2/tributary/issues/49) | No equalizer or audio-filter configuration. | GStreamer DSP design plus explicit behavior for every output backend. |
| [#143 — Import and pull-sync server-native Subsonic playlists](https://github.com/jm2/tributary/issues/143) | Implementation is complete in the current PR pending creation: pull-only reads, exact-session authority, link persistence, atomic mirrors, durable sidebar ordering, reconnect coordination, capability-filtered virtualized browsing, opaque Import Copy/Keep Synced tokens, and visible localized recovery controls all pass their deterministic validation. GitHub still reports the issue open before that PR is created and merged. | Open and merge the current implementation PR with `Closes #143`. Later real-server field validation may identify dialect-specific bugs, but no additional planned implementation record remains for this accepted scope. |
| [#46 — Drag and drop](https://github.com/jm2/tributary/issues/46) | Column-header reordering exists; track/file drag-and-drop does not. | Local playlist DnD first; file export, remote rows, and device copies as distinct policies. |
| [#39 — Album art in browser](https://github.com/jm2/tributary/issues/39) | Artwork is shown for now-playing, not in the Genre/Artist/Album browser. | Virtualized art UI with bounded async cache, cancellation, accessibility, and authenticated art. |
| [#29 — UI refinement](https://github.com/jm2/tributary/issues/29) | Requested separators/alignment changes are not implemented. | Split into independently reviewable visual changes after current-theme design review. |
| [#14 — Browse by folder](https://github.com/jm2/tributary/issues/14) | Browser panes expose Genre, Artist, and Album only. | Root-relative folder model and lazy UI with multiple-root and unavailable-root semantics. |
| [#11 — Offline cache/download](https://github.com/jm2/tributary/issues/11) | No remote download or offline catalogue subsystem. | Large persistent cache/download epic with quota, retry, reconciliation, and secure auth handling. |
| [#8 — Android synchronization](https://github.com/jm2/tributary/issues/8) | Mounted-device browse/play exists; transfer, sync, automount, and MTP do not. | Large transfer/sync epic with MTP, write authority, planning, progress, conflicts, and rollback. |

## AirPlay 2

AirPlay 2 receivers advertise via `_airplay._tcp.local.` and discovery labels them as `airplay2`,
but the UI deliberately filters them out because Tributary currently has only an AirPlay 1/RAOP
sender through GStreamer's `raopsink`.

No sender dependency or protocol implementation has been selected. The implementation must first
confirm current reverse-engineering and interoperability details for:

1. receiver pairing and authenticated session establishment;
2. the encrypted control channel;
3. codec, transport, and timing requirements for audio; and
4. clock synchronization only if multi-device playback is included.

Candidate approaches are delegation to a maintained external sender, a pure-Rust in-tree or
`gst-plugins-rs` sender, or waiting for an upstream component to mature. The single-binary
distribution model, platform packaging, license, maintenance health, and real-device test matrix
must be part of that decision. This work should start with a design issue; it is not currently an
active implementation.

## Other explicit follow-ups and accepted limits

### Engineering follow-ups

- Re-evaluate the unmaintained `paste` and `proc-macro-error2` dependency paths and the inactive,
  lockfile-only RSA advisory by 2026-10-10 or the next release, and immediately if MySQL support is
  ever enabled.
- Remove the macOS GStreamer channel-cap workaround only after an upstream fix is available in the
  supported runtime floor and has been validated on affected multi-channel hardware.
- A direct end-to-end watcher-backlog/root-confirmation ordering harness would strengthen existing
  component and engine-loop coverage, although the remediation acceptance record is already closed.

### Deliberate current limitations, not scheduled commitments

- Tributary does not automount or eject volumes and cannot browse pathless/MTP-only devices.
- Markerless read-only library roots cannot be enrolled because Tributary cannot create the durable
  identity marker required by the current fail-closed trust model.
- Direct Apple/iTunes XML, Google Takeout CSV, M3U, and service-specific playlist imports are not
  implemented. The documented conversion to XSPF is the current interoperability path, and fuzzy
  “similar name” matching is intentionally avoided.
- OS-open delivery admits the first valid playable candidate. A multi-file ephemeral queue is a
  possible future extension, not a committed feature.
- A stronger platform-native removable file identifier and explicit saved-server endpoint rebind
  are possible future schema extensions, not scheduled work.
- Proper Apple code signing/notarization and the intentionally deferred release-workflow exercise
  are distribution/release work, not product implementation in this roadmap.

## Keeping this roadmap current

When an item becomes active:

1. create or refine its GitHub issue with scope, acceptance criteria, non-goals, and dependencies;
2. assign a priority and milestone rather than treating this proposed order as a commitment;
3. link the design document when protocol, schema, authority, or cross-output behavior is involved;
4. update this roadmap, README feature status, and `CHANGELOG.md` in the implementing PR; and
5. close or narrow the issue only when the shipped behavior and documentation match.

[#8]: https://github.com/jm2/tributary/issues/8
[#11]: https://github.com/jm2/tributary/issues/11
[#14]: https://github.com/jm2/tributary/issues/14
[#29]: https://github.com/jm2/tributary/issues/29
[#37]: https://github.com/jm2/tributary/issues/37
[#39]: https://github.com/jm2/tributary/issues/39
[#46]: https://github.com/jm2/tributary/issues/46
[#47]: https://github.com/jm2/tributary/issues/47
[#49]: https://github.com/jm2/tributary/issues/49
[#50]: https://github.com/jm2/tributary/issues/50
[#57]: https://github.com/jm2/tributary/issues/57
[#140]: https://github.com/jm2/tributary/pull/140
[#141]: https://github.com/jm2/tributary/pull/141
[#142]: https://github.com/jm2/tributary/pull/142
[#143]: https://github.com/jm2/tributary/issues/143
