# Tributary active implementation backlog

Last audited: 2026-07-20

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
- Deliver a large record through explicitly documented, reviewable PR slices when needed, but keep
  its checkbox open until the complete acceptance contract lands. Split the checklist record only
  when the resulting parts are independently complete and countable. Keep the numerator and
  denominator synchronized with the literal top-level checkboxes.
- Scope protocol, schema, authority, cross-output, and privacy decisions in a design document or
  refined GitHub issue before committing to an implementation.
- Do not treat the order below as a release promise. It is a dependency-aware starting order and
  can change as issues receive product decisions and milestones.

Current status: **12/38 (31.6%)** active implementation records complete. This percentage measures
checklist completion, not equal engineering effort: several P3 records are deliberately large
epics. The archived remediation remains **220/223 (98.7%)** complete; its three open records are
real-environment validation, not missing implementation.

## Current focus

P1.1, P1.2, all three P1.3 playback-history records, both P1.4 rating records, and P1.5 through
server-native link persistence and the atomic pull-sync engine are complete. Record E is now being
delivered in reviewable UI/lifecycle slices. Its structural groundwork replaces translated-label
and backend-string identity with typed header/playlist state, prevents ordinary edit affordances
from reaching linked mirrors, and reserves a separate localized, accessible footer shell for
durable sync/recovery state instead of overloading the continually updated track-count label.
Its follow-up durable revision lane makes every current playlist-sidebar mutation producer
converge through strictly increasing complete joined snapshots rather than scan replacements plus
partial CRUD callbacks. SQLite triggers cover parents, links, cascades, and raw writes to those
tables; a lifecycle-owned publisher coalesces hints, polls for lost hints, and GTK rejects equal or
older delivery.

Continue with Record E's headless latest-request operation coordinator, then the virtualized
Import Copy/Keep Synced browser and Sync Now/conflict/missing/offline recovery flows. The accepted
[`subsonic-playlist-sync.md`](subsonic-playlist-sync.md) contract keeps this capability pull-only and
separate from Tributary's ordinary mixed-source playlists. Smart playlists and XSPF import/export
remain local-only, while mixed-source metadata export requires its own no-locator policy.

The independent Linux watcher correctness fix tracked in
[#103](https://github.com/jm2/tributary/pull/103) does not change the **12/38** feature total.
The salvaged scope rejects explicitly classified access/access-time noise before the bounded watcher
queue without filtering real bootstrap mutations or backend errors, while retaining overflow
evidence for authoritative reconciliation. It intentionally omits the original persistent
unparseable-file cache so transient parser and I/O failures remain retryable. It is tracked outside
the 38-record feature backlog.

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

  Implemented delivery boundary: the context menu snapshots the active source together with its
  selection. In [#133](https://github.com/jm2/tributary/pull/133), only the exact built-in `local`
  view could enter the playlist database path; every non-local or malformed view presented a
  localized all-or-none explanation first. Existing Add/Remove/Properties labels used their
  shipped translations, and all 13 locale catalogs carried non-fallback result copy. P1.5 Record B
  now admits current authenticated Subsonic, Jellyfin, Plex, and DAAP selections through exact
  registry authority while retaining that refusal for unsupported or unavailable selections and
  discarding stale selection results.

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
- [x] Add accessible editing, display, sorting, and smart-playlist rules, with explicit behavior for
  read-only or rating-incapable sources
  ([#139](https://github.com/jm2/tributary/pull/139)).

  Implemented foundation: [`ratings.md`](ratings.md) defines a canonical whole integer from 1
  through 100, with `None` as unrated, and one coherent Writable, ReadOnly, or Unsupported state per
  track. Tributary owns and transactionally writes ratings only for exact local-library IDs in
  SQLite; migration 12 leaves every legacy row `NULL`, enforces integer/range storage, validates
  interrupted upgrades even after later columns are appended, and supports down/up retry.
  Existing-row metadata refreshes and recognized paired watcher file/directory renames preserve
  ratings; an offline or otherwise unrecognized
  remove-plus-add becomes a new unrated row. Embedded tags are neither read nor written for this
  field.

  Subsonic's valid integer 1–5 `userRating` maps read-only to 20-point increments. Jellyfin and Plex
  accept only finite native user ratings in 0–10, round the tenfold value, and preserve native zero
  as canonical 1; malformed, absent, or numerically unrepresentable values are read-only unrated
  without rejecting their response. DAAP, radio, removable, external, and unknown sources are
  Unsupported, all remote mutations fail closed, and catalogue
  admission rejects per-track/source capability disagreement. XSPF v1 intentionally emits no
  rating and ignores rating-like metadata on import; playlist matching never mutates library
  ratings. A future metadata transfer requires separate opt-in conflict handling.

  Implemented UI and rule contract: the exact integer Rating column is visible by default and
  remains configurable and reorderable. Existing profiles expose it once through a versioned
  column-config migration; a current profile that intentionally hides it stays hidden. Writable
  local rows use a keyboard-operable localized popover to set 1–100 or clear to unrated. Admission
  is synchronous and FIFO with playback-history writes, no value changes optimistically, and only
  a committed exact-ID result replaces the local row and invalidates cached or active playlist
  projections. A failed write shows fixed localized copy while database details remain internal.
  Read-only sources render an exact value or unrated state explicitly as read-only; unsupported
  rows render Unavailable and neither state offers an editor. Radio-Browser's intentionally compact
  station view omits Rating alongside its other track-only metadata while retaining Unsupported
  capability in the model.

  Column sorting keeps rated rows first in both directions, orders exact values normally, then
  readable-unrated before unsupported, with stable `TrackId` ties. Smart playlists now expose
  Rating equality, inequality, strict greater/less, inclusive range, Is Rated, and Is Unrated;
  readable missing values match no numeric predicate (including Is Not), unsupported values match
  neither numeric nor presence predicates. Invalid or reversed editor input is retained with
  localized visible/accessibility feedback while OK stays disabled; malformed externally serialized
  values fail closed during evaluation. Rating sort and Highest/Lowest Rated limit selection keep
  missing values last in either direction and use stable-ID ties. Existing serialized rules and
  migration-11 fingerprints remain unchanged. The complete contract and validation matrix are in
  [`ratings.md`](ratings.md).

### P1.5 — Persist source-scoped playlists

- [x] Design and migrate regular playlist entries from local track foreign keys to stable
  source-scoped `(SourceId, TrackId)` identity, with deterministic local migration, ordering,
  duplicate-occurrence, unavailable-source, deletion, and rollback behavior
  ([contract](source-scoped-playlists.md); [#47](https://github.com/jm2/tributary/issues/47);
  [#140](https://github.com/jm2/tributary/pull/140)).

  Implemented storage contract: migration 13 preserves every valid predecessor occurrence ID,
  playlist ID, position, duplicate, fingerprint, and local path-evidence field while assigning the
  exact built-in local `source_id`. Canonical `(source_id, track_id)` identity is separated from a
  nullable `local_track_id -> tracks(id) ON DELETE SET NULL` cache, so a remote native ID is never
  forced through the local table. `track_id` remains nullable only for unmatched local imports with
  usable path or normalized title-and-artist evidence. Typed source-generic storage rejects
  non-local path evidence and persists no URL, credential, lease, or session epoch; existing local
  Add/load, XSPF import/export, reconciliation, rename, root-reauthorization, and deletion behaviors
  remain the compatibility boundary for this slice.

- [x] **Record A — Live catalogue authority:** establish the live-registry and accepted-catalogue
  authority foundation for source-scoped regular-playlist entries. Capability must default to
  unsupported and opt in only authenticated Subsonic, Jellyfin, Plex, and DAAP adapters. Ordered
  lookup must accept only the exact current source session, catalogue generation, and native-track
  identities, returning no locator, credential, lease, or route. Its closed result may carry the
  non-secret session epoch and catalogue generation transiently; neither becomes playlist storage.
  Guarded media resolution remains a separate at-use operation with retained private authority
  ([#141](https://github.com/jm2/tributary/pull/141)).

  Implemented contract: `ManagedSourceAdapter` exposes an
  explicit `Unsupported` or `SourceScopedEntries` capability. The default and Radio-Browser,
  removable-media, and ephemeral external-file adapters remain unsupported; only the four retained
  authenticated catalogue adapters opt in. Registry lookup returns one ordered resolution per
  requested occurrence. Unsupported sources, unavailable sessions, and missing exact tracks return
  fixed unavailable results independently without erasing valid neighbors. If an otherwise
  accepted catalogue contains a missing or duplicate native identity, its frozen regular-playlist
  authority index is `Invalid` and every requested occurrence for that source fails closed; the
  catalogue may remain available to existing non-playlist UI. Repeated requested IDs remain ordered
  duplicate occurrences. Available rows use a dedicated display/sort/rating/history metadata
  whitelist rather than a `Track` clone. The lifecycle snapshot owns a separately revocable
  generation lease, so replacement or teardown invalidates guarded media even while an observer
  retains an old snapshot clone; raw adapter failures become fixed media-error categories.
  Catalogue projection runs on the captured immutable snapshot outside the lifecycle mutex, then
  exact pointer identity and both leases are rechecked before return or adapter work. Revalidation
  denies a guard made stale by refresh, replacement, retirement, release, or shutdown.
  The capability does not authorize a database write, UI row, playback request, or source-native
  playlist mutation.

  Coverage pins the default-deny and four-opt-in matrix, `Invalid` indexing, ordered
  duplicate requests, all four closed unavailable reasons, locator-free metadata, current-result
  rechecks, predecessor retention, refresh/replacement invalidation, unlocked selector re-entry,
  selector-time stale-work denial, synchronous lifecycle denial, and
  membership/capability/epoch/generation checks before and after stream or artwork resolution.

- [x] **Record B — Mixed-source UI integration:** integrate the registry authority foundation into
  regular-playlist Add, Remove, rendering, and Play behavior, with explicit disconnected/missing
  states, source retirement, stale-epoch rejection, occurrence ordering, duplicates, and all-or-none
  multi-selection tests ([#142](https://github.com/jm2/tributary/pull/142)).

  Implemented contract: Add to Playlist accepts exact local rows plus current authenticated
  Subsonic, Jellyfin, Plex, and DAAP catalogue rows. It resolves the complete ordered remote
  selection through Record A. After staging its SQL inserts and immediately before commit, that
  transaction revalidates the complete result and acquires exact session/catalogue permits. A
  result made stale during staging rolls back; after admission, a concurrent refresh, replacement,
  disconnect, or shutdown waits for commit or rollback. The transaction and permits transfer to an
  independent completion worker, so cancellation cannot strand authority or abandon commit
  completion. Any unsupported, unavailable, missing, or invalid-catalogue member likewise writes
  nothing. Support is
  never inferred from a source key, backend label, cached GTK row, or persisted
  fingerprint. Radio-Browser, removable media, ephemeral external files, and unknown sources remain
  unsupported and receive the localized all-or-none refusal.

  Regular rendering retains every durable occurrence in position order, including duplicates and
  unavailable rows. Local rows use exact current database metadata; authenticated remote rows use
  only Record A's sanitized current catalogue projection. Disconnected or retired sources,
  unsupported owners, invalid catalogues, missing native tracks, and missing/unmatched local
  identities produce explicit localized rows that remain removable. Stale projection work or
  results are discarded and the playlist is invalidated and projected again from current
  authority; a stale guard is denied rather than rendered as a row state. No persisted fingerprint
  is shown as display metadata or used to guess a replacement. Remove uses exact durable entry IDs
  in one transaction, so duplicate occurrences are independently addressable and a failure changes
  nothing.

  A playlist remains a `ViewOrigin`; each queue item separately retains its real media-owning
  source. Local stream and embedded-art access retains exact file authority, while authenticated
  remote stream and artwork resolution revalidates the closed catalogue guard at use. Source
  refresh, replacement, retirement, disconnect, or shutdown denies stale work, and reconnection
  restores availability only for the same exact `(SourceId, TrackId)`. Only exact local occurrences
  contribute to local playback history. Remote ratings remain live read-only or unsupported and
  cannot be mutated through playlist membership. Smart playlists and XSPF import/export remain
  local-only; attempting to export a regular playlist with any remote or unresolved occurrence is
  refused all-or-none before the destination is touched. Mixed-source metadata export and
  Subsonic server-native synchronization remain separate policy records.

- [x] **Record C — Server-native contract, protocol, and pull authority:** define Subsonic native
  playlist direction, identity, conflict, offline, deletion, unsupported-feature, and unlink
  semantics; implement bounded `getPlaylists`/`getPlaylist` reads; and expose them only through an
  exact-current-session, default-deny registry capability
  ([contract](subsonic-playlist-sync.md); [#143](https://github.com/jm2/tributary/issues/143)).

  Implemented foundation: Import Copy is a one-time detached editable snapshot, while Keep Synced
  is an opt-in read-only, server-authoritative pull mirror. The contract excludes
  server create/update/delete calls and periodic polling. Exact native playlist IDs use a
  content-redacted 4 KiB identity; names and owners are bounded presentation hints, advertised
  counts are non-authoritative, and detail snapshots preserve exact ordered track IDs including
  duplicates. List/detail response bodies and element counts are bounded and reject malformed,
  oversized, duplicate-playlist-ID, or detail-ID-mismatch responses all-or-none.

  `ManagedSourceAdapter` defaults the capability to `Unsupported`; only an authenticated Subsonic
  adapter opts into `PullSnapshots`. Registry list/detail calls capture the exact source session,
  run network work outside the lifecycle lock, and recheck adapter identity, epoch, and revocable
  lease afterward. Disconnect, replacement, retirement, shutdown, or reuse of predecessor
  operation proof against a successor session rejects stale results. Returned errors are fixed adapter-unsupported,
  lifecycle-unavailable, or closed backend categories and expose no URL, credential, server text,
  response body, or native ID. Playlist endpoint membership deliberately grants no catalogue or
  playback authority. This
  record adds no migration, playlist/link write, sync scheduler, or UI.

- [x] **Record D — Link persistence and atomic pull synchronization:** add dedicated non-secret
  native-playlist link state plus detached Import Copy and read-only Keep Synced manager operations.
  Preserve exact order and duplicates; apply each current pull all-or-none; detect local drift
  before overwrite; retain the last successful snapshot on offline, parse, auth, cancellation, or
  stale-session failure; and represent server deletion without cascading local data.

  Migration 14 adds one strictly validated, pull-only link per exact `(SourceId,
  NativePlaylistId)`, separate from regular-playlist entries and with no URL, credential, locator,
  route, epoch, lease, owner, advertised count, or raw failure. It stores the effective synchronized
  name, frozen SHA-256 ordered-membership digest, last-success timestamp, orthogonal clean/conflict
  and present/missing state, and a monotonic revision. Downgrade refuses while any link exists, so
  an older binary cannot silently turn a mirror into an editable playlist.

  Import Copy commits a detached editable regular playlist with no link. Keep Synced atomically
  creates one unique read-only mirror; successful pulls preserve exact order and duplicates,
  replace name/membership all-or-none, and keep occurrence IDs on a name-only update. Every pull or
  complete-list absence for an existing mirror starts from a pre-network revision ticket and
  compare-and-swaps that exact
  revision, preventing a late result from overwriting newer durable state. Local name or membership
  drift records conflict without overwriting the last complete snapshot; explicit Replace Local
  uses a fresh current pull. Complete-list absence changes only server-presence/local-drift state,
  while detail/backend/auth/parse/cancellation/stale failures have no persistence path. Unlink
  retains the local copy; explicit Remove Local Copy deletes it transactionally. Ordinary rename,
  delete, Add, Remove, reorder, smart-rule mutation, and reconciliation cannot mutate a linked
  mirror.

  A successful list/detail now carries an opaque exact-session receipt rather than a reusable raw
  guard. Only exact list presence can select a detail; only exact complete-list absence can mint
  deletion evidence. Immediately before commit, the registry revalidates its exact incarnation,
  source, adapter, epoch, capability, and active session lease and returns an operation-bound,
  session-only permit. Persistence verifies that it was minted for the same sealed pull or absence
  result and retains it through commit or rollback; another current operation cannot substitute its
  authority. Reconciliation excludes linked mirrors with a zero-bind subquery instead of one SQLite
  host parameter per link. Staleness before admission rolls back; replacement,
  disconnect, or shutdown after admission waits. This authority deliberately does not require or
  grant catalogue/playback membership. At the Record D delivery boundary, Record E still owned UI,
  localization, reconnect scheduling, and the in-memory latest-request generation lane; its first
  structural slice is recorded immediately below.

- [ ] **Record E — Server-native playlist UI and lifecycle integration:** add localized Import Copy,
  Keep Synced, Sync Now, conflict/missing/offline status, reconnect refresh, Retry, Replace Local
  with Server, Unlink, and Remove Local Copy flows with accessible end-to-end coverage. Do not make
  linked mirrors editable or expose unsupported adapters/servers as writable playlist sources.

  Structural groundwork is complete in
  [#146](https://github.com/jm2/tributary/pull/146) without claiming the Record E checkbox.
  Sidebar section and playlist identity are typed rather than inferred from localized
  display text or compatibility backend strings. A single ordered playlist/link snapshot makes
  link presence win even over a damaged smart-playlist flag, rejects malformed link state instead
  of exposing an editable row, and keeps native playlist identity out of GTK objects, actions, and
  diagnostics. Pull mirrors receive explicit read-only/conflict/missing presentation and are
  excluded from Rename, Export, Delete, Edit Smart Rules, Add, and Remove paths; persistence remains
  the final defense.

  Ordinary Create, Rename, Delete, and smart-rule operations now publish sidebar changes only for
  a closed committed result and show fixed localized failure copy otherwise. Smart-playlist
  creation writes its validated rule payload and compatibility columns atomically, eliminating the
  intermediate rule-less row. The track footer now has a distinct initially hidden status shell
  with deterministic state priority, recovery-action slots, recycled-state reset, and complete
  non-fallback copy in all 13 catalogs; the existing count/duration label remains independent and
  no timestamp is presented as proof of freshness.

  This slice deliberately performs no listing, pull, reconnect scheduling, or server mutation. Its
  follow-up publication slice adds migration 15's exact singleton revision and six SQLite triggers
  over playlist parents and server-playlist links. Effective inserts, updates, deletes, cascades,
  and raw writes to either domain table advance inside their own transaction; no-op updates and
  rollbacks do not. Startup revalidates the exact derived table and trigger ownership. One
  lifecycle-owned publisher reads
  revision plus the complete redacted join in a coherent transaction, coalesces post-commit hints,
  polls the durable revision for lost hints, and emits the first valid Ready or versioned
  Unavailable snapshot and thereafter only a strictly newer one. GTK applies its first snapshot and
  then only a newer one, replacing or retracting the whole section, suppressing intermediate
  selection navigation, and selecting structural Local when the active playlist disappears.
  Partial Create/Rename/Delete/Import row patches are removed. Reversed delivery, equal-version
  idempotence, malformed joined state, restart, raw domain-table SQL, cascades, rollback,
  exhaustion, and publisher owner-close and blocked-output cancellation are covered, closing
  create/import erasure, rename reversion, delete resurrection, and stale link-classification
  races.

  The next slice is a GTK-free source/remote/local-keyed coordinator with exact-session capability,
  latest-request generation, cancellation, final admission, reconnect deduplication, retirement,
  and shutdown-drain coverage. The final slice then connects that coordinator to a virtualized
  browser and the Import Copy, Keep Synced, Sync Now, Retry, Replace, Unlink, and Remove Local Copy
  controls before Record E can be checked.

  Structural-slice validation: the locked suite passes 20 library, 1,197 application, and 10
  repository-metadata tests (1,227 total). Strict all-target/all-feature Clippy is green in debug
  and release profiles; Rust 1.92 all-target checking, formatting, whitespace checks, exact
  13-catalog/20-key locale parity, typed-identity scans, and an independent privacy/documentation
  audit are also green.

  Publication-slice validation: the locked suite passes 20 library, 1,223 application, and 10
  repository-metadata tests (1,253 total). Strict all-target/all-feature Clippy is green in debug
  and release profiles; Rust 1.92 all-target checking, formatting, whitespace checks, focused
  migration/projection coverage, and independent code/privacy/documentation audits are also green.

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
| 2026-07-19 | P1.4 rating ownership and persistence foundation | [#138](https://github.com/jm2/tributary/pull/138) | Defined the canonical 1–100 value and coherent Writable/ReadOnly/Unsupported capabilities; added constrained nullable migration 12, transactional exact-local-ID set/clear, scan preservation, validated read-only Subsonic/Jellyfin/Plex conversion, fail-closed catalogue invariants and remote writes, and an explicit rating-neutral XSPF/import policy. Accessible editing, display, sorting, and smart rules were tracked in the following P1.4 record. |
| 2026-07-19 | P1.4 rating UI and smart-playlist rules | [#139](https://github.com/jm2/tributary/pull/139) | Added the exact Rating column with localized cell/editor states, keyboard-operable local set/clear, honest read-only/unavailable states, serialized post-commit exact-ID refresh, failure-safe feedback, versioned column-config exposure, null-last deterministic sorting, and capability-aware numeric/presence smart rules plus Highest/Lowest Rated selection. This completed P1.4. |
| 2026-07-19 | P1.5 source-scoped regular-playlist storage | [#140](https://github.com/jm2/tributary/pull/140) | Added migration 13 and typed atomic storage around exact `(SourceId, TrackId)` occurrence identity, a separate nullable local foreign-key cache, exact schema/index recognition, lossless-or-refused downgrade, local compatibility, and explicit no-locator/no-credential boundaries. It intentionally reserved live authority, mixed-source UI, and Subsonic-native synchronization for separate records. |
| 2026-07-19 | P1.5 live-catalogue playlist authority | [#141](https://github.com/jm2/tributary/pull/141) | Added default-deny adapter capability, exact ordered catalogue lookup with invalid-catalogue rejection and sanitized metadata, transient epoch/generation guards, closed media errors, and lifecycle-owned generation leases revoked independently of retained snapshots. It intentionally reserved Add/Remove/render/Play consumption for Record B. |
| 2026-07-19 | P1.5 mixed-source regular-playlist UI | [#142](https://github.com/jm2/tributary/pull/142) | Integrated exact local and authenticated Subsonic/Jellyfin/Plex/DAAP entries through all-or-none Add, durable-occurrence Remove, ordered duplicate-preserving projection with explicit removable unavailable rows, and per-source guarded Play/artwork. Add revalidates after staging SQL, rolls back a stale result, and retains exact session/catalogue permits through an admitted commit or rollback. Current closed unavailable reasons render honestly; stale projection results are discarded/reprojected and stale guards are denied. Local history and remote rating ownership remain unchanged. Smart playlists and XSPF remain local-only; mixed/unresolved XSPF export refuses all-or-none instead of truncating, while mixed-source metadata export and Subsonic-native synchronization remain separate. |
| 2026-07-19 | P1.5 Subsonic server-playlist contract and pull authority | [#144](https://github.com/jm2/tributary/pull/144) | Defined detached Import Copy and read-only pull-mirror semantics, including conflict, offline, deletion, unlink, privacy, and non-mutation boundaries. Added bounded/redacted native playlist identity and exact ordered snapshots, authenticated `getPlaylists`/`getPlaylist` reads, and a Subsonic-only default-deny capability whose opaque guard and pre/post lifecycle checks reject disconnect, replacement, retirement, shutdown, and successor-session reuse. At that delivery boundary no persistence, synchronization commit, or UI was included; Records D and E remained open. |
| 2026-07-19 | P1.5 Subsonic link persistence and atomic pull engine | [#145](https://github.com/jm2/tributary/pull/145) | Added strict migration 14 and a redacted typed link with unique exact server identity, separate local-conflict/server-presence state, frozen membership digest, last-success metadata, and revision CAS. Import Copy stays detached; Keep Synced creates one read-only mirror. Exact-session pull/absence receipts acquire an operation-bound commit permit after SQL staging; persistence rejects a permit from any other pull or absence result, stale work rolls back, and invalidation waits for an admitted atomic commit. Reconciliation excludes mirrors through a zero-bind subquery that remains safe beyond SQLite's host-parameter limit. Pull, conflict, Replace, missing, unlink, explicit removal, ordinary-mutation denial, downgrade refusal, failure retention, and exact order/duplicate behavior are deterministic. UI, reconnect scheduling, localization, and latest-request operation generations remain Record E. |
| 2026-07-19 | P1.5 server-playlist UI structural groundwork | [#146](https://github.com/jm2/tributary/pull/146) | Added typed/redacted joined sidebar identity, read-only/conflict/missing mirror presentation, ordinary-action exclusion with transactional revalidation, commit-only CRUD outcomes, atomic smart creation/rule updates, stale-load and recycled-row defenses, structural Local fallback, and a hidden accessible recovery shell with exact copy in all 13 catalogs. At that merge boundary, Record E still retained the globally ordered full-sidebar lane, headless exact-session coordinator, reconnect/browser/action wiring, and end-to-end coverage. |
| 2026-07-20 | P1.5 durable playlist-sidebar publication | Pending PR | Added migration 15's exact singleton revision and six transactional triggers, startup schema revalidation, a coherent redacted full-snapshot publisher with coalesced hints and polling fallback, and a strictly ordered GTK reducer with selection-safe replacement and structural fallback. Partial CRUD/import patches are removed; Record E remains open for the headless latest-request coordinator and reconnect/browser/recovery wiring. |
| 2026-07-18 | Linux watcher feedback-loop fix | [#103](https://github.com/jm2/tributary/pull/103) | Narrowed the external proposal to filter self-generated access events before queue admission without filtering genuine startup events or backend errors; bounded overflow still drives authoritative reconciliation. Persistent negative parse caching is deliberately excluded so failures remain retryable; this separate correctness fix does not advance the feature numerator. |
