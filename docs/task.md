# Tributary active implementation backlog

Last audited: 2026-07-21

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

Current status: **14/38 (36.8%)** active implementation records complete. This percentage measures
checklist completion, not equal engineering effort: several P3 records are deliberately large
epics. The archived remediation remains **220/223 (98.7%)** complete; its three open records are
real-environment validation, not missing implementation.

## Current focus

P1.1 through P1.5 and the Rhythmbox half of P2.1 are complete. The Rhythmbox migration is a bounded,
preview-first local workflow with exact non-guessing path matching, explicit metadata policy,
actionable safe-subset acknowledgement, conservative exactly representable smart-playlist import,
transactional stale-state revalidation, and a content-free idempotency receipt. The accepted
[`rhythmbox-migration.md`](rhythmbox-migration.md) contract records its privacy, limit,
cancellation, retry, and intentional-omission boundaries.

Continue with P2.1's Last.fm integration, the highest-priority unchecked record whose dependencies
are satisfied. The accepted [`lastfm-scrobbling.md`](lastfm-scrobbling.md) contract fixes its
desktop authorization, vault-only account authority, opt-in and per-source privacy policy,
authoritative now-playing/scrobble evidence, 10,000-row account-bound FIFO, retry classification,
disconnect purge, and shutdown boundary. In addition to the bounded protocol client, retryable
native-vault boundary, strict migrations 17/18, a transactional account-bound FIFO and durable
pause gate, and missing-vault recovery authority, an internal runtime slice now owns account
binding, serialized bounded ingress,
oldest-first one-flight delivery, durable retry/terminal settlement, same-account reauthorization,
and disconnect/shutdown barriers. It is deliberately not wired into production application or UI
lifecycle yet. Continue with generation-owned playback threshold and now-playing evidence, browser
authorization and localized consent/source policy, account/status UI and localization, application
startup/shutdown integration, and package-time credentials. Production API-key registration and
package-time key/secret injection remain explicit external release prerequisites rather than
reasons to weaken development behavior. Smart playlists and XSPF import/export remain local-only,
while mixed-source metadata export still requires its own no-locator policy.

The independent Linux watcher correctness fix tracked in
[#103](https://github.com/jm2/tributary/pull/103) does not change the **14/38** feature total.
The salvaged scope rejects explicitly classified access/access-time noise before the bounded watcher
queue without filtering real bootstrap mutations or backend errors, while retaining overflow
evidence for authoritative reconciliation. It intentionally omits the original persistent
unparseable-file cache so transient parser and I/O failures remain retryable. It is tracked outside
the 38-record feature backlog.

The cross-platform release-component containment work implemented in PR #152 on 2026-07-20 likewise does not
change the feature total. It removes the observed path by which broad Windows/macOS GStreamer
bundling could pull unused optical-disc access and decryption components into an artifact, adds a
single reviewed deny policy, and makes Windows, macOS, native Linux, and Flatpak payload validation
fail closed. Review and CI follow-up also pinned production macOS inspection inputs, covered
nonstandard Mach-O and ELF reference paths, made Windows source-copy and final PE gates complete
across DLL/DRV/EXE module forms, and distinguished FFmpeg's non-decrypting `libbluray` dependency
from the denied `gstbluray`,
AACS, and BD+ components. Its exact scope and limitations are recorded in
[`release-component-policy.md`](release-component-policy.md). P2.1 Last.fm remains the feature
focus after this urgent distribution safeguard. The same provenance review established that no
current supported GStreamer/Homebrew/MSYS2 package provides the claimed `raopsink` element; its
incorrect package-install guidance is now honest, and the existing P2.4 sender-design records cover
selecting and validating a maintained AirPlay path.

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
  refreshes by stable ID and active/cached playlist projections are invalidated. The gated AirPlay
  1 seam contributes the same evidence through generation-scoped 500 ms position samples only when
  an external compatible sender is registered. The complete contract is in
  [`playback-history.md`](playback-history.md).

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

- [x] **Record E — Server-native playlist UI and lifecycle integration:** add localized Import Copy,
  Keep Synced, Sync Now, conflict/missing/offline status, reconnect refresh, Retry, Replace Local
  with Server, Unlink, and Remove Local Copy flows with accessible end-to-end coverage. Do not make
  linked mirrors editable or expose unsupported adapters/servers as writable playlist sources
  ([#149](https://github.com/jm2/tributary/pull/149), which closes
  [#143](https://github.com/jm2/tributary/issues/143)).

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

  The structural slice itself deliberately performs no listing, pull, reconnect scheduling, or
  server mutation. Its
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

  The GTK-free lifecycle slice is now implemented. One owner provides disjoint typed source,
  exact `(SourceId, NativePlaylistId)`, and durable local-playlist lanes. A coordinator-global
  request stamp is reserved before reconnect discovery and reused for fan-out, so delayed
  reconnect work cannot supersede newer manual intent. Newer work cancels only a pre-admission
  predecessor; a same-key successor waits for both an admitted task and its move-only guard to
  settle before it prepares durable state, while unrelated keys remain concurrent. Final
  coordinator admission is the request-order linearization point.

  `SourceRegistry` lists only through the exact session epoch observed in an atomic lifecycle
  baseline. Each newly accepted `(SourceId, session_epoch)` schedules at most one sweep: all exact
  revision tickets are prepared before one complete list, native presence/absence is indexed, and
  no more than eight local detail/commit operations run concurrently. Exact presence alone selects
  detail; only exact absence from a successful complete list can mark a mirror missing. Listing or
  detail failure writes nothing. The headless Sync Now, Retry, Replace, Unlink, and Remove Local
  Copy facade shares the local lane and returns only closed, content-free completion categories.

  Manager admission occurs after SQL staging. Pull, Replace, and missing work retain the move-only
  coordinator guard together with exact source-session commit authority through detached commit or
  rollback. Source-independent Unlink and Remove Local Copy use the same post-staging coordinator
  admission. Shutdown closes coordinator admission before source revocation, cancels only work
  which has not been admitted, and uses a persistent barrier to drain admitted tasks and guards.
  Durable revision CAS remains the restart and persistence-order backstop.

  The final browser and visible-action slice is complete. The Playlists header exposes a localized
  **Server Playlists…** entry whose dialog uses a virtualized `GtkListView` and lists only current
  sources whose exact active session advertises `PullSnapshots`. GTK retains the existing
  `SourceId`, bounded localized name/owner hints, and broker-minted opaque tokens; native playlist
  identity, exact selections, session receipts, coordinator keys, and commit authority remain
  Tokio-owned. Browser listing has its own latest-only cancellation lane, so it cannot supersede
  reconnect recovery. One active snapshot is bounded by the protocol's 10,000-playlist ceiling;
  reload, source lifecycle change, dialog close, or shutdown revokes every unused predecessor
  token.

  Import Copy and Keep Synced atomically consume one exact token only after the browser's
  eight-action capacity gate accepts it; a Busy settlement preserves the token for retry. Detail
  fetch and persistence use the exact broker-held selection and remote-playlist coordinator lane.
  Existing manager transactions stage SQL before jointly acquiring coordinator admission and
  exact registry commit authority, and only a committed detached import or unique read-only mirror
  requests the durable full-sidebar publisher. Same-name server playlists therefore remain exact
  distinct identities, accepted tokens cannot be replayed, stale sessions write nothing, and
  browser close/shutdown drains every admitted action without exposing server-controlled content
  in completion values or diagnostics.

  Selecting a pull mirror now renders the localized status shell from its typed durable
  clean/conflict and present/missing state plus a content-redacted, in-memory inspection of current
  pull authority. Inspection performs no server listing or health probe. Five targetless window
  actions resolve the currently selected durable local playlist again at activation: Sync Now,
  Retry, Replace Local with Server, Unlink, and Remove Local Copy. Network actions fail closed when
  exact pull authority is unavailable, while Unlink and Remove Local Copy remain available
  offline. Replace, Unlink, and Remove require localized confirmation. Selection, source lifecycle,
  sidebar snapshot, inspection, and operation generations reject stale or exhausted work; action
  sensitivity follows the same presentation plan as the visible controls, and focus moves to a
  stable status or track-list target when a running, revoked, or removed state hides the focused
  control.

  This completion preserves the pull-only boundary: Tributary still performs no server-playlist
  create, update, or delete call, no periodic server polling, no fuzzy title/artist merge, no
  non-Subsonic server-playlist operation, and no native playlist-ID transfer into GTK. Accessible
  end-to-end coverage is deliberately layered: real registry/coordinator/database/sidebar broker
  flows are combined with deterministic GTK presentation, action-authority, localization,
  recycled-state, generation, and focus-policy tests plus structural accessibility review. It does
  not claim a live assistive-technology/display harness in CI.

  Structural-slice validation: the locked suite passes 20 library, 1,197 application, and 10
  repository-metadata tests (1,227 total). Strict all-target/all-feature Clippy is green in debug
  and release profiles; Rust 1.92 all-target checking, formatting, whitespace checks, exact
  13-catalog/20-key locale parity, typed-identity scans, and an independent privacy/documentation
  audit are also green.

  Publication-slice validation: the locked suite passes 20 library, 1,223 application, and 10
  repository-metadata tests (1,253 total). Strict all-target/all-feature Clippy is green in debug
  and release profiles; Rust 1.92 all-target checking, formatting, whitespace checks, focused
  migration/projection coverage, and independent code/privacy/documentation audits are also green.

  Coordinator/reconnect-slice validation: 71 focused server-playlist tests pass, including
  deterministic same-key admitted-drain ordering, atomic direct-request ordering against delayed
  reconnect fan-out, and pending-manual completion classification. Separate real empty-, one-, and
  nine-mirror coordinator/registry/database/sidebar integrations prove that an unlinked source
  performs no server-playlist list, while linked sources preserve exact presence, detail-failure
  retention, complete-list absence, one shared listing, and a measured eight-operation cap that
  holds a ninth exact-ID mirror until one slot finishes. Locked debug and release suites each pass
  20 library, 1,249 application, and 10 repository-metadata tests (1,279 total). Strict
  all-target/all-feature Clippy is green in debug and release profiles; Rust 1.92 all-target
  checking, formatting, whitespace checks, and independent code/privacy/documentation audits are
  also green.

  Final Record E validation: 92 focused server-playlist tests pass, including capability gating,
  independently cancelled latest-only listing, exact same-name identity, one-shot token replay and
  session revocation, eight-action capacity with retryable Busy, shutdown drain, in-memory link
  inspection, visible recovery priority/action authority, destructive-confirmation ABA rejection,
  stale completion rejection, generation exhaustion, and focus-safe state replacement. All 13
  catalogs contain the exact 40 server-playlist keys without English fallback. Locked debug and
  release suites each pass 20 library, 1,270 application, and 10 repository-metadata tests (1,300
  total). Formatting, whitespace checks, and independent implementation/privacy/documentation
  review are green.

## P2 — User-facing integrations and bounded enhancements

### P2.1 — Migration and listening integrations

- [x] Import Rhythmbox `rhythmdb.xml`, playlists, play counts, and ratings transactionally and
  idempotently, with exact non-guessing matching and actionable conflict/unmatched reporting
  ([#57](https://github.com/jm2/tributary/issues/57),
  [#150](https://github.com/jm2/tributary/pull/150)).

  Completed scope:

  - The chooser accepts a stable non-link local profile directory and captures only its exact direct
    children through retained, revalidated regular-file handles. Strict UTF-8 XML 1.0 parsing and
    independent byte, depth, scalar, song, playlist, entry, issue, query, and mapped-path budgets
    fail closed on unsafe, expanding, or incoherent input.
  - An optional component-exact root remap precedes exact current local-path matching. Ratings and
    monotonic play counts are explicit policy; last-played timestamps and destructive rating
    replacement remain opt-in. Titles, artists, albums, filenames, and fuzzy similarity never
    establish identity.
  - Nine independently capped report categories retain 100 deterministic details apiece and exact
    omitted counts. Local paths/names appear only as escaped preview text, while diagnostics and
    stored receipts stay content-free. Any skipped, conflicted, invalid, duplicate, or path-only
    result gates Apply behind explicit acknowledgement.
  - Playlist-name conflicts are the first and sole playlist-level planning reason reported for that
    complete source playlist; they suppress additional queue, unsupported-rule, or static-occurrence
    detail without suppressing independent parser issues. Otherwise, static playlists preserve
    order and duplicates, while queues and inexact automatic rules are reported and skipped; only a
    flat, exactly equivalent play-count/rating subset without explicit sort or active limit is
    imported. The three validated Rhythmbox browser/search presentation attributes are recognized
    as membership-inert rather than rejecting ordinary saved playlists, and are excluded from
    semantic receipt identity so UI-only changes do not create a new attempt.
  - One move-only plan revalidates path membership, track values, and every incoming playlist-name
    presence inside the write transaction. Writes are all-or-none, the minimal three-field receipt
    is inserted last, and an exact/concurrent retry is a no-op. A committed first apply attempts one
    coherent library/sidebar refresh; any incomplete refresh on a live UI lane returns a typed,
    localized committed-but-restart-required result without exposing internal error details.
    Cancellation stops bounded capture and suppresses stale preview results; an admitted apply
    drains before shutdown Flush.

  Final validation: 75 focused Rhythmbox tests pass; exact code/catalog/placeholder parity covers
  125 keys in all 13 locales without substantive English fallback. Locked debug and release suites
  each pass 20 library, 1,345 application, and 10 repository-metadata tests (1,375 total). Strict
  Clippy is green in both profiles, the declared Rust 1.92 toolchain passes the locked all-target
  check, and formatting/diff checks are clean.
- [ ] Implement Last.fm authorization and protected secret storage, now-playing/scrobble thresholds,
  durable retry/offline behavior, privacy UX, and source-aware metadata on authoritative playback
  events ([contract](lastfm-scrobbling.md); [#50](https://github.com/jm2/tributary/issues/50);
  [foundation #151](https://github.com/jm2/tributary/pull/151);
  [runtime/lifecycle slice](https://github.com/jm2/tributary/pull/153)).

  Acceptance criteria:

  - Enable the feature only when a Last.fm API key and shared secret were injected at build time;
    keep production registration/injection external, ship no placeholder or runtime credential
    input, and show an honest unavailable capability when either build value is absent.
  - Use Last.fm's desktop browser flow with a latest-only, in-memory, 60-minute request token and a
    one-shot session exchange. Store only the returned session key and username plus one random
    opaque account UUID in the operating-system credential vault, with no plaintext fallback;
    create/startup-read/delete failure disables request and queue admission. An exact same-account
    code-9 update failure may retain offline queue admission for that already-valid binding while
    its durable reauthentication marker keeps network delivery stopped.
  - Require explicit localized consent before authorization. Local, removable, and structured
    external-file occurrences may participate after opt-in; each authenticated Subsonic,
    Jellyfin, Plex, or DAAP source remains off until separately enabled, mixed playlists retain the
    real source owner's policy, and Radio-Browser remains unconditionally excluded.
  - Freeze only structured `Track` metadata: required artist/title and optional album/album artist
    are independently capped at 1,024 UTF-8 bytes and control-free, duration must be known and
    greater than 30 seconds, and no filename, URI, fuzzy match, lookup, or Last.fm correction may
    change the payload. Omit parameters for which Tributary has no authoritative value.
  - Attempt now-playing exactly once at the first accepted generation-owned Playing evidence and
    never retry it. Admit one scrobble only after observed forward playback reaches
    `min(ceil(duration / 2), 240 seconds)`; pause, buffering, seeks, restarts, retries, stale events,
    wall time, and natural end cannot manufacture credit.
  - Commit qualified scrobbles before network use to one account-bound SQLite FIFO capped globally
    at 10,000 rows. Persist only Last.fm payload fields plus opaque identity/order/binding and
    bounded retry state; at capacity refuse the new row visibly without evicting old history.
  - Send only the oldest rows, in batches of at most 50, with at-least-once semantics. Retry
    timeouts/transport failures, HTTP 429/5xx without a recognized provider error envelope,
    transient service codes 8/11/16, and rate-limit code 29 with durable capped backoff; code 9
    retains the queue and pauses for same-account reauthorization. Accepted, ignored, and every
    other recognized error are terminal, and malformed item mapping quarantines rather than
    guesses. Never apply corrections.
  - Disconnect closes admission, retires in-flight work, drains admitted database commands,
    atomically purges every queued row while installing a binding-only cleanup tombstone, and clears
    that marker only after exact vault deletion. Cleanup remains closed and retryable across either
    cross-store failure. Normal shutdown drains admitted queue writes but neither waits indefinitely
    for network I/O nor deletes a row without a committed terminal result.
  - Cover migration/downgrade, exact queue and metadata limits, authorization/vault failure,
    source/session ownership, playback discontinuities, every response class, offline restart,
    ambiguous at-least-once replay, disconnect/shutdown races, redaction, accessibility, and exact
    key/placeholder parity across all 13 shipped locale catalogs without contacting public Last.fm
    infrastructure in CI.

  Current internal implementation (the countable record intentionally remains open):

  - Added an HTTPS-only, redirect-safe signed client for desktop token/session exchange,
    now-playing, and ordered 50-row scrobble requests. Typed response classification is
    content-free; unknown provider codes and incoherent item mappings quarantine rather than
    guess. Provable worst-case form and JSON-echo fixtures pin 1 MiB request and 2 MiB response
    caps.
  - Added a versioned native-vault record with exact RFC 4122 v4 account UUID, nonblank control-free
    username, and 32-hex session validation. Secret Service, Keychain, and Credential Manager are
    selected explicitly per operation with no plaintext fallback or sticky failed initializer;
    SQLite receives only the domain-separated SHA-256 account binding. Flatpak grants only the
    reviewed `org.freedesktop.secrets` session-bus name.
  - Added migration 17 and a strictly validated private FIFO. Atomic admission enforces one account,
    exact occurrence idempotency, and the 10,000-row cap. Every idempotent hit revalidates the
    complete stored row before returning its identity, so malformed identities or retry state are
    retained and fail closed instead of returning corrupt authority. Destructive recovery for that
    state while its vault account remains valid is not exposed by this internal slice. Opaque batch
    receipts make terminal settlement and durable rescheduling all-or-none compare-and-swap
    operations over the exact oldest prefix. Generated SeaORM active models and worker-facing
    models cannot print private metadata. Migration 18 upgrades an already-applied migration-17
    database with an exact private
    singleton containing only the same one-way binding and a fixed reauthentication,
    compatibility, capability, or credential-cleanup category. Receipt- or account-checked pause
    writes that succeed commit before worker stop/publication; a failed pause write closes ingress,
    stops delivery, and reports a fixed capability/storage failure without claiming restart
    durability. Startup restores the exact committed phase without a worker.
    Code 9 clears only after same-account reauthorization, while compatibility/capability pauses
    require an opaque exact-runtime/account/revision/category recovery command. Disconnect
    atomically empties the queue and installs the cleanup tombstone, then clears that exact marker
    only after the matching vault record is deleted or proven already absent. Cleanup restart is
    sessionless and admits neither queue nor network work. Missing/corrupt-vault recovery separately
    purges both retained tables, and downgrade refuses either retained state.
  - Added normal binding-scoped purge plus a separate missing/corrupt-vault recovery primitive that
    requires opaque proof of closed and FIFO-drained admission and deletes only the captured row-ID
    snapshot, including corrupt non-positive identities. The process-global lifecycle owner proves
    that barrier before it can issue the capability.
  - Added a single serialized runtime owner whose public enqueue boundary accepts only validated
    unbound payloads. The runtime ingress gate attaches the active vault account binding before it
    sends the bound command to that owner. Its bounded metadata ingress reserves control capacity
    for delivery and lifecycle markers, linearizing enqueue, reauthorization, disconnect, and
    shutdown without letting callers forge account authority.
  - Added one generation-owned, non-mutating worker which reads the exact oldest FIFO prefix, sends
    at most 50 rows with only one request in flight, transfers its opaque receipt to the runtime,
    and cannot inspect a successor batch until the actor acknowledges durable handling. Typed
    disposition settles accepted/ignored and recognized permanent failures, quarantines malformed
    cardinality or unclassifiable results, and durably retries only timeouts/transport failures,
    HTTP 429/5xx without a recognized provider error envelope, provider codes 8/11/16/29 with 30-second
    exponential backoff capped at one hour. Code 9 retains
    the queue and permits only exact same-account vault reauthorization before delivery restarts.
  - SQLite settlement is the commit point: aggregate accepted/ignored/rejected counters advance
    only after the exact receipt is deleted transactionally. Remote acceptance followed by actor or
    process loss retains the byte-exact row for at-least-once replay by a successor runtime; stale
    worker generations can neither settle rows nor alter current status.
  - Disconnect closes admission, cancels and joins in-flight delivery, drains every earlier
    admitted command, atomically purges the exact account queue while installing a cleanup
    tombstone, and only then deletes the exact vault record and clears that marker. Either
    cross-store failure remains cleanup-only and retryable across restart without retaining a
    session or reopening authority.
    Shutdown closes admission and proves its FIFO drain while cancelling network work without
    deleting an unsettled receipt. A process-global vault lease prevents overlapping owners from
    racing native credentials, and startup exposes explicit closed/drained recovery for a missing
    or corrupt vault record rather than silently discarding private rows.
  - Worker panics are supervised into a typed content-free capability failure without deleting the
    receipt. Actor unwind retains the owner and vault lease while it closes ingress, cancels and
    joins delivery, then attempts to commit or validate a capability pause for any still-unpurged
    account before releasing the lease. If SQLite cannot establish that pause, the shutdown proof
    remains failed rather than claiming a durable commit. The process-wide panic hook omits every
    panic payload. Private metadata, credentials, provider bodies, receipt contents, exact
    durations, and panic payloads remain absent from status and diagnostics.
  - Validation passes 127 focused Last.fm tests. The focused matrix includes exact 10,000-row
    capacity/contention and full-refusal behavior, every accepted/ignored/permanent/transient/
    cardinality disposition including provider codes 8/11/16/29, durable backoff across restart,
    same-account code-9 recovery, stale generations, ambiguous accepted-before-settlement replay,
    disconnect/shutdown and staged-restart races, process-global vault ownership, cleanup crash
    windows, missing/corrupt recovery, durable pause/restart/manual-clear and restart-visible backoff
    state, actor-panic quiescence, and panic/duration redaction boundaries.
    Locked debug and release suites each pass 20 library, 1,473 application, and 14
    repository-metadata tests (1,507 total), alongside strict debug and release Clippy, the Rust
    1.92 locked all-target check, formatting and diff checks, and the dependency audit.

  Remaining production work: this internal runtime is not yet instantiated by application startup
  or joined by application shutdown. Add a production activation/unavailable-capability issuer;
  structured `Track` eligibility and source-owner conversion; generation-owned threshold,
  now-playing, queue-admission, and rate-limited-toast integration; latest-only browser
  authorization; explicit consent and per-source policy; same-account reauthorization and
  different-account replacement/purge flows; disconnect, missing/corrupt/valid-vault queue recovery,
  queue-full, account, and status UX; complete localization and accessibility; package credential
  injection, verification, and production API registration; and the remaining end-to-end and
  platform acceptance matrix.

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
- [ ] Open and complete an AirPlay sender design investigation that first resolves the current
  non-shipped `raopsink` seam, then scopes maintained RAOP and/or AirPlay 2 dependencies, pairing,
  encrypted control, audio/timing, licensing, key provenance, packaging, and real-device tests.
- [ ] Implement the selected maintained AirPlay sender path without presenting unsupported
  discovered receivers as playable; keep simultaneous multi-room sync out of scope unless
  separately approved.
- [ ] Validate the selected AirPlay interoperability and packaging paths on supported platforms and
  representative receivers, including reconnect, cancellation, authentication failure, and
  actionable diagnostics.
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
- A capability-derived Windows/macOS GStreamer audio-plugin allowlist and narrower native Linux
  plugin-package relationships are possible stronger least-privilege packaging boundaries, not a
  scheduled feature record. They require a real cross-platform container/source/output playback
  matrix before replacing the current shared fail-closed deny policy safely.

## Implementation log

| Date | Task | PR | Result |
|---|---|---|---|
| 2026-07-21 | P2.1 Last.fm durable delivery and lifecycle internals | [#153](https://github.com/jm2/tributary/pull/153) | Added runtime-owned account binding and bounded serialized ingress; exact oldest-first one-flight batches of at most 50; typed terminal and durable transient retry handling, including provider codes 8/11/16/29; migration 18's restart-stable reauthentication/compatibility/capability gates plus its sessionless credential-cleanup tombstone and exact-runtime/account/revision/category recovery; exact same-account code-9 reauthorization; post-SQLite counters and accepted-before-settlement replay; disconnect/shutdown barriers; process-global vault ownership and missing/corrupt recovery; and content-redacted worker/actor panic supervision that joins predecessor work before releasing vault authority. This slice remains deliberately unwired from production application/UI lifecycle, so [#50](https://github.com/jm2/tributary/issues/50) and the 14/38 P2.1 record remain open. |
| 2026-07-20 | Cross-platform release-component containment | [#152](https://github.com/jm2/tributary/pull/152) | Replaced permissive disc-component bundling with one shared deny policy; filtered before native dependency traversal; rejected denied transitive dependencies and recognizable path references; rejected link-based, test-hook, and incomplete-inspection escapes; and added final Windows ZIP/PE, macOS, native Linux, Packit/COPR, and complete Flatpak app-commit gates, including stale-tree and installer-only paths. Review regressions cover nonstandard Mach-O placement, all bracket-valued ELF dynamic tags plus the program interpreter, source-copy reparse points, Windows DLL/DRV/EXE import forms, failure-cleaned temporary state, and the deliberate `libbluray`/decryptor distinction. The dependency audit also removed false `gst-plugins-bad`/`raopsink` install guidance and folded maintained AirPlay sender selection into P2.4. Ordinary codecs and transport cryptography remain intentionally available. This distribution safeguard does not advance the 14/38 feature numerator. |
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
| 2026-07-20 | P1.5 durable playlist-sidebar publication | [#147](https://github.com/jm2/tributary/pull/147) | Added migration 15's exact singleton revision and six transactional triggers, startup schema revalidation, a coherent redacted full-snapshot publisher with coalesced hints and polling fallback, and a strictly ordered GTK reducer with selection-safe replacement and structural fallback. Partial CRUD/import patches were removed; the headless coordinator and final browser/recovery consumer were explicit follow-on work later completed by [#148](https://github.com/jm2/tributary/pull/148) and [#149](https://github.com/jm2/tributary/pull/149). |
| 2026-07-20 | P1.5 server-playlist coordinator and reconnect lifecycle | [#148](https://github.com/jm2/tributary/pull/148) | Added typed GTK-free source/remote/local lanes with global reconnect/manual ordering, monotonic generations, pre-admission supersession, same-key waiting through admitted task and guard settlement, and unrelated-key concurrency. Reconnect binds one sweep to each exact accepted epoch, prepares revisions before one indexed complete list, and bounds local detail/commit fan-out to eight; detail failure writes nothing and only proven complete-list absence marks missing. Pull/missing persistence retains coordinator plus source authority after SQL staging, local Unlink/Remove uses the same guarded lane, and shutdown closes admission before source revocation and drains admitted work. At that merge boundary the redacted headless Sync/Retry/Replace/Unlink/Remove completion surface was ready; [#149](https://github.com/jm2/tributary/pull/149) adds its final browser and visible-action consumer. |
| 2026-07-20 | P1.5 server-playlist browser and visible recovery UI | [#149](https://github.com/jm2/tributary/pull/149) | Added the capability-filtered virtualized browser, independently cancellable listing, bounded revocable opaque tokens, exact Import Copy/Keep Synced admission, visible generation-gated recovery controls, destructive confirmations, focus-safe accessibility behavior, and complete 13-catalog localization. This completes Record E and P1.5 and closes [#143](https://github.com/jm2/tributary/issues/143). |
| 2026-07-20 | P2.1 Rhythmbox profile migration | [#150](https://github.com/jm2/tributary/pull/150) | Added strict bounded profile capture/parsing, exact-path policy and conservative smart translation, nine-category acknowledged preview reporting, transactional stale-state revalidation, minimal semantic-digest receipts, one-shot publication, localized lifecycle UI, and end-to-end/idempotency/privacy regressions. This completes [#57](https://github.com/jm2/tributary/issues/57); Last.fm remains the next P2.1 record. |
| 2026-07-20 | P2.1 Last.fm protocol/vault/queue foundation | [#151](https://github.com/jm2/tributary/pull/151) | Added the bounded signed client, strict native-vault record and retryable platform stores, migration 17, transactional one-account FIFO with full-row validation of idempotent hits and exact batch receipts, narrowly reviewed Flatpak Secret Service access, closed/drained missing-vault recovery, and generated-model redaction. The top-level [#50](https://github.com/jm2/tributary/issues/50) record remains open for playback/runtime, delivery lifecycle, UI, localization, and package credential injection. |
| 2026-07-18 | Linux watcher feedback-loop fix | [#103](https://github.com/jm2/tributary/pull/103) | Narrowed the external proposal to filter self-generated access events before queue admission without filtering genuine startup events or backend errors; bounded overflow still drives authoritative reconciliation. Persistent negative parse caching is deliberately excluded so failures remain retryable; this separate correctness fix does not advance the feature numerator. |
