# Changelog

All notable changes to Tributary are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Server-native playlist pulls now have a GTK-free latest-request coordinator and reconnect
  runtime** ([#148](https://github.com/jm2/tributary/pull/148)) — A lifecycle-owned owner serializes
  starts and final admission across three typed, content-redacted lanes: one source, one
  source/native remote playlist, or one durable local playlist. A newer request cancels a same-key
  predecessor only before admission. If the
  predecessor is already admitted, its successor waits until both the operation task and its
  move-only admission guard settle; unrelated keys continue concurrently. Checked per-key
  generations fail closed on exhaustion. A separate coordinator-global request stamp is reserved
  before reconnect discovery and reused by its delayed local fan-out. Direct requests reserve and
  enqueue atomically against stamped fan-out submission, so a newer manual request for the same
  mirror cannot be overwritten by older reconnect work.
  The source-lifecycle observer schedules exactly one sweep for each newly accepted
  `(SourceId, session epoch)`, ignoring catalogue-only invalidations within that session. Each sweep
  captures every durable link revision before network I/O, reads one complete listing from the
  exact observed session, uses its indexed exact-presence or sealed-absence evidence, and fans out
  at most eight local operations at once. Manual Sync Now, Retry, Replace Local with Server,
  Unlink, and Remove Local Copy enter those same local lanes through a completion facade whose
  status and diagnostics contain no playlist identity or server-controlled content. A pending
  operation displaced before it starts reports superseded, while a started task lost unexpectedly
  reports interrupted.
  Pull and missing-state transactions stage SQL first, then jointly acquire the coordinator guard
  and exact sealed registry commit authority immediately before persistence; both remain held
  through commit or rollback. Unlink and local removal now have the same post-staging coordinator
  admission guard. Superseded or rejected work rolls back without a sidebar hint, while applied,
  conflict, missing, unlink, and removal outcomes request the durable full-snapshot publisher.
  Normal shutdown closes coordinator admission before source shutdown, cancels only
  pre-admission work, and drains admitted tasks and guards through a cloneable persistent barrier.
  This remains pull-only: it adds no server mutation, periodic polling, fuzzy title/artist merge,
  non-Subsonic server-playlist authority, or native playlist IDs in GTK. P1.5 Record E remains open
  for the virtualized server-playlist browser, opaque Import Copy/Keep Synced action tokens, and
  visible localized, accessible GTK recovery/action wiring. Validation passes 71 focused
  server-playlist tests plus real one- and nine-mirror reconnect integrations; the latter measures
  one shared list, eight blocked exact-ID operations, a held ninth, and all nine commits after
  release. Locked debug and release suites each pass 20 library, 1,248 application, and 10
  repository-contract tests (1,278 total), with strict all-target/all-feature Clippy in both
  profiles, Rust 1.92 checking, formatting, whitespace, and independent code/privacy/documentation
  review green.
- **Playlist sidebar publication is now durably ordered and full-snapshot only**
  ([#147](https://github.com/jm2/tributary/pull/147)) — Migration 15
  adds one exact singleton SQLite revision and six guarded triggers over playlist parents and
  server-playlist links. Effective inserts, updates, deletes, foreign-key cascades, and raw SQL
  against either domain table advance the revision inside the writer's transaction; no-op updates
  do not, and rollback cannot leak an increment. Startup revalidates the exact derived table and
  trigger set so a missing, replaced, or extra trigger fails closed instead of silently weakening
  ordering.
  A lifecycle-owned publisher reads the revision and complete redacted playlist/link join in one
  read transaction, coalesces explicit post-commit refresh hints, and periodically polls the
  durable revision to recover from lost hints and non-UI writers. Read/schema failures retain the
  last applicable snapshot for retry; a valid revision with malformed joined model data publishes
  a versioned unavailable state instead of retaining stale editable rows. The first valid snapshot
  is published; thereafter only strictly newer snapshots leave the publisher.
  GTK now accepts its first versioned snapshot and then only a strictly newer one. A ready snapshot
  replaces the whole playlist section; unavailable state clears it and selects structural Local
  when the active playlist disappears. Intermediate selection signals are suppressed during row
  replacement, then only the final retained playlist or structural fallback may navigate. Equal or
  older delivery is ignored. Committed Create, Import, Rename, Delete, smart creation, and smart-rule
  updates request a complete refresh; scan/default-seeding hints remain revision-gated even when no
  write committed.
  The former partial row insertion/rebind/removal callbacks are gone, so a delayed scan can no
  longer erase a create/import, revert a rename, resurrect a delete, or roll back a mirror-state
  classification. At that PR's delivery boundary the slice performed no server listing, pull
  scheduling, browser work, or recovery action. The headless listing and reconnect coordinator is
  now documented above; browser and visible recovery actions remain in P1.5 Record E.
- **Server-native playlist UI now has fail-closed structural and localization groundwork**
  ([#146](https://github.com/jm2/tributary/pull/146)) —
  Sidebar sections and playlists carry typed identities instead of treating translated labels or
  compatibility backend strings as authority. The library engine publishes playlists and optional
  pull links from one deterministic joined snapshot; a valid link always wins over the parent
  smart flag, malformed link state rejects the complete publication, and diagnostics redact both
  mirrored names and native identities. GTK retains only the local playlist ID, durable
  clean/conflict and present/missing state, and bounded display text. Linked mirrors render as
  read-only or warning rows and expose none of the ordinary Rename, Export, Delete, Edit Smart,
  Add, or Remove affordances; activation-time checks and the existing transactional manager guards
  remain authoritative if a cached row changes.
  Playlist Create, Rename, Delete, and smart-rule UI workers now return a closed committed/failed
  outcome. A sidebar row is inserted, rebound, or removed only after its database transaction
  commits, while every failure uses fixed localized copy and leaves the visible model unchanged.
  Smart-playlist creation serializes and validates the complete rule/limit representation before
  inserting it in one transaction, so a process failure cannot leave a published rule-less smart
  playlist. Default smart-playlist seeding uses the same atomic path.
  The existing track-count footer is now a real status-bar row with a separate initially hidden
  server-playlist status shell. Its pure priority plan distinguishes syncing, combined
  conflict/missing, missing, conflict, failure, offline, and clean read-only states; reserves
  Sync Now, Retry, Replace Local with Server, Unlink, and Remove Local Copy controls; disables an
  offline Retry; resets recycled action/accessibility state; and never presents a last-success time
  as freshness. All 13 locale catalogs provide the exact visible, confirmation, tooltip, and
  accessibility keys with tests that reject missing or English-fallback values. The shell remains
  unwired and hidden at that PR's delivery boundary. The headless coordinator above now supplies
  the operation and reconnect lifecycle, but the browser and visible GTK actions remain open. A
  follow-up durable
  revision lane now orders complete joined snapshots across scans, CRUD, cascades, and link-state
  writers; this structural change itself performs no server listing, pull, reconnect scheduling,
  polling, or mutation.
- **Subsonic server-native playlists now have durable pull-only links and an atomic sync engine**
  ([#145](https://github.com/jm2/tributary/pull/145)) — Migration 14 adds an exact-shape
  `server_playlist_links` table with one unique mirror per canonical source/native playlist pair,
  a fixed read-only pull mode, bounded effective synchronized name, versioned 32-byte SHA-256
  ordered-membership digest, UTC-millisecond last-success timestamp, orthogonal clean/conflict and
  present/missing state, and a monotonic revision. It stores no endpoint, path, username,
  credential, token, locator, source route, owner, advertised count, raw failure, adapter, epoch,
  lease, or operation generation. Existing regular playlists and entries are unchanged; parent
  deletion cascades the link, while downgrade refuses until every link is explicitly removed so an
  older binary cannot silently make mirrors editable.
  Import Copy atomically creates an ordinary editable regular playlist and never writes a link.
  Keep Synced creates one read-only mirror with exact server order, duplicate occurrences, and
  source-scoped track IDs even when current catalogue membership is absent. Pull applies a fresh
  name and membership all-or-none, preserves occurrence IDs for a name-only update, clears current
  state on success, and retains the previous snapshot on every rejected/failed operation. Before
  network work on an existing mirror, callers capture a typed link revision; pull, conflict, and
  complete-list missing updates compare-and-swap and increment that exact revision so a late result
  cannot overwrite newer durable state. Initial import/mirror creation has no prior revision. A
  frozen membership digest plus a separate byte-exact synchronized-name
  comparison detects local drift. Normal pull records conflict without overwriting; Replace Local
  deliberately applies a fresh server snapshot. Server absence requires sealed evidence from a
  successful complete list, preserves entries/name/digest/last-success metadata, and can coexist
  with local conflict. Detail, transport, authentication, parsing, cancellation, and stale-session
  failures have no deletion or persistence path. Unlink retains an editable local copy; explicit
  Remove Local Copy deletes the playlist and link transactionally.
  Ordinary rename, delete, Add, Remove, reorder, smart-rule mutation, and reconciliation now reject
  linked mirrors inside their write transaction; reconciliation uses a zero-bind link subquery, so
  large mirror collections cannot exceed SQLite's host-parameter limit. Successful list/detail
  operations carry opaque weak exact-session receipts rather than exposed epochs or reusable
  guards. Immediately before a database commit, `SourceRegistry` revalidates the exact registry
  incarnation, source, adapter, session epoch, capability, and active lease and returns a
  session-only permit bound to that exact sealed pull or absence result. Persistence rejects an
  authority minted for any other operation,
  including another current source or pull, and retains a matching permit through commit or
  rollback. A stale result rolls back; replacement, disconnect, or shutdown after
  admission waits. This authority neither depends on nor grants music-catalogue/playback authority.
  The new raw link entity and validated link, ticket, copy, preparation, and outcome diagnostics
  redact server-controlled native identity, synchronized names, and digests.
  At that PR's delivery boundary, Import Copy/Keep Synced/Sync Now actions, reconnect scheduling,
  localized conflict/missing/offline recovery, and latest-request operation generations remained
  staged. The headless coordinator and reconnect runtime are now documented above; the browser and
  visible GTK action/recovery wiring remain. The engine still performs no server playlist mutation
  or periodic polling.
- **Subsonic server-native playlists now have a pull-only contract and exact-session read
  foundation** ([#144](https://github.com/jm2/tributary/pull/144)) — The accepted
  [`docs/subsonic-playlist-sync.md`](docs/subsonic-playlist-sync.md) contract separates a future
  one-time detached, editable **Import Copy** from an opt-in read-only, server-authoritative
  **Keep Synced** mirror. It defines all-or-none application, local-drift conflicts, offline and
  cancellation retention, reconnect/manual refresh, server rename/deletion, explicit
  Replace/Unlink/Remove recovery, and no-locator/no-credential persistence before schema or UI
  work. Synchronization is pull-only: this capability never creates, updates, or deletes a server
  playlist and does not add periodic polling.
  A new exact, content-redacted `NativePlaylistId` uses the 4 KiB remote-identity ceiling.
  `ServerPlaylistSummary` and `ServerPlaylistSnapshot` bound optional name/owner hints to 16 KiB,
  treat advertised song counts as non-authoritative hints, and preserve exact ordered `TrackId`
  occurrences including duplicates. Subsonic now reads authenticated `getPlaylists.view` and
  `getPlaylist.view?id` through the existing reverse-proxy-aware client with 8 MiB/10,000-summary
  and 64 MiB/100,000-entry limits. Explicit empty listings and zero-entry playlist details remain
  valid; a missing/null list wrapper, missing detail object, malformed or oversized member,
  duplicate native playlist ID, excessive response, or detail-ID mismatch rejects the complete
  operation without truncation.
  Server-playlist request diagnostics, architecture DTO debug output, and registry-returned errors
  expose fixed categories and safe sizes/counts rather than URLs, credentials, server text,
  response bodies, or native IDs.
  Because Subsonic dialects conflate unsupported endpoints and missing entities, an HTTP or failed-
  envelope rejection remains a closed backend failure and is never treated as an empty successful
  list. The persistence engine above accepts missing state only from exact absence in a successful
  complete list.
  `ManagedSourceAdapter` defaults server-native playlists to `Unsupported`; only authenticated
  Subsonic opts into `PullSnapshots`. Registry list/detail operations capture the exact adapter,
  session epoch, and revocable lease, perform network I/O outside the lifecycle mutex, and recheck
  the same authority afterward. Disconnect, replacement, retirement, shutdown, final release, or
  reuse of predecessor list/detail proof against a successor session rejects the result. The
  persistence engine above tightens this proof into sealed presence/absence receipts and final
  commit authority. Playlist endpoint
  membership does not require accepted music-catalogue membership and deliberately grants no
  display, stream, artwork, rating, or history authority. At that delivery boundary the foundation
  added no database migration, playlist/link write, synchronization scheduler, or UI. Persistence
  and the atomic engine are now documented above. At that PR's delivery boundary the scheduler and
  UI remained staged; the headless scheduler/coordinator is now implemented above, while the
  virtualized browser and visible GTK actions remain in Record E.
- **Regular playlists now have a source-scoped storage foundation**
  ([#140](https://github.com/jm2/tributary/pull/140)) — Migration 13 makes the exact
  `(source_id, track_id)` pair canonical for each durable regular-playlist occurrence while keeping
  the playlist itself a view rather than a media source. Every valid existing row is
  deterministically assigned to the built-in local `SourceId`; entry and playlist IDs, order,
  duplicate occurrences, nullable track identity, normalized match metadata, and imported local
  path evidence are preserved byte-for-byte. A separate nullable
  `local_track_id -> tracks(id) ON DELETE SET NULL`
  cache retains local deletion and reconciliation integrity without requiring a remote native ID
  to reference the unrelated local table. `track_id` may be absent only for an unmatched local
  import with usable path or normalized title-and-artist evidence, populated local caches must agree
  with canonical identity, and non-local entries must have an exact bounded native ID with neither
  a local cache nor file-path evidence.
  The SQLite table rebuild, data copy, index/constraint restoration, and foreign-key validation are
  one transaction; an interrupted or forced failure restores the complete predecessor definition
  and remains retryable. The migrator distinguishes the exact predecessor and target definitions
  instead of accepting a partial lookalike. Downgrade round-trips representable local rows but
  transactionally refuses any non-local or otherwise inexpressible state rather than dropping it,
  converting it to local, or guessing by metadata.
  Typed source-generic storage accepts only stable source/native identity plus optional non-secret
  normalized fingerprints, adds no display/source-label snapshots, rejects non-local path evidence,
  preserves same-ID tracks from different sources as distinct media, and stores no server
  URL, stream/artwork locator, credential, lease, route, or session epoch. Existing manual local
  Add/load, XSPF import/export, duplicate ordering, local reconciliation, track deletion, rename,
  and root-reauthorization paths retain their prior behavior through the new schema. At this
  storage slice's delivery boundary the UI deliberately remained local-only; the live authority
  and mixed-source UI records below now consume the schema without widening what it may persist.
  Mixed-source XSPF metadata export remains separately designed. The server-native Subsonic pull
  contract, read authority, and separate link persistence are documented above; user-facing
  server-native playlist UI remains staged separately.
  The complete boundary and validation matrix are in
  [`docs/source-scoped-playlists.md`](docs/source-scoped-playlists.md).
- **Regular playlists now have default-deny live-catalogue authority**
  ([#141](https://github.com/jm2/tributary/pull/141)) —
  `ManagedSourceAdapter` exposes one explicit `Unsupported` or `SourceScopedEntries` capability. The
  default, Radio-Browser, removable-media, ephemeral external-file, and unknown adapters remain
  unsupported; only retained authenticated Subsonic, Jellyfin, Plex, and DAAP catalogues opt in.
  Each accepted source payload freezes that capability with its complete catalogue. Authority
  lookup returns one ordered resolution per occurrence against the exact current `SourceId`,
  non-secret session epoch, accepted catalogue generation, capability, and native-track membership.
  An otherwise accepted catalogue with a missing or duplicate native ID receives an `Invalid`
  playlist-authority index, so every playlist occurrence for it fails closed without choosing a
  duplicate while existing non-playlist UI may continue using the catalogue. Repeated requested IDs
  remain repeated ordered occurrences. Missing exact tracks, unsupported sources, and unavailable
  sessions receive fixed unavailable results independently; revalidation denies guards made stale
  by replacement, refresh, retirement, release, or shutdown.
  Returned track metadata uses a dedicated display/sort/rating/history whitelist rather than a
  `Track` clone, so file paths, file/network URLs, stream and artwork locators, credentials, leases,
  routes, raw backend failures, and future unreviewed fields cannot cross implicitly. Its closed
  authority guard may carry the non-secret session epoch and catalogue generation transiently, but
  neither is written into playlist storage. Immutable catalogue projection runs outside the
  lifecycle mutex, then exact snapshot identity and both leases are rechecked before adapter work;
  this keeps a selector from re-entering or extending the critical section without widening stale
  authority. Guarded stream and artwork resolution rechecks exact membership, capability, epoch,
  and generation again after the asynchronous result returns, then maps raw adapter errors to fixed
  categories. A lifecycle-owned generation lease is explicitly revoked on snapshot
  replacement/removal and every teardown path, so retained
  snapshot clones cannot keep an already returned request active.
  Connecting or a failed replacement preserves the accepted predecessor; successful replacement
  or same-session catalogue refresh invalidates old guards. Disconnect, shutdown, and final source
  release synchronously deny new authority before asynchronous teardown completes.
  This PR's delivery boundary was an internal authority foundation rather than a UI authorization;
  the mixed-source record immediately below is its reviewed consumer. Mixed-source XSPF export
  remains separately tracked. At that PR's delivery boundary server-native Subsonic UI/reconnect
  integration was also deferred; the current headless reconnect state and remaining GTK work are
  documented above.
- **Regular playlists now integrate exact mixed-source entries end to end**
  ([#142](https://github.com/jm2/tributary/pull/142)) — Add to Playlist now accepts exact local
  tracks plus rows from current authenticated Subsonic, Jellyfin, Plex, and DAAP catalogues. One
  registry lookup validates every selected remote occurrence against its current source, session
  epoch, accepted catalogue generation, advertised capability, and native-track membership before
  one atomic ordered write. After staging its SQL changes and immediately before commit, the
  transaction revalidates the complete batch and acquires exact commit-scoped session/catalogue
  authority permits. A result made stale during staging is rejected and the transaction rolls back;
  once admitted, refresh, replacement, disconnect, or shutdown waits for commit or rollback. The
  transaction and permits transfer to an independent completion worker, so caller cancellation or
  a synchronous lifecycle revoker cannot strand authority or starve the commit. If
  any current selected member is unsupported, disconnected, missing,
  or in an invalid catalogue, a localized all-or-none result is shown and nothing is written.
  Radio-Browser,
  removable-media, ephemeral external-file, and unknown sources remain unsupported.
  Regular-playlist projection now retains every durable entry in position order, including
  duplicates and unavailable occurrences. Available local rows use the exact current database
  record; available remote rows use only the registry's sanitized live metadata. Disconnected or
  retired sources, unsupported owners, invalid catalogues, missing native tracks, and
  missing/unmatched local identities render explicit localized unavailable rows that remain
  removable. Stale projected work or results are discarded and the affected playlist is
  invalidated and projected again from current authority. Persisted fingerprints are neither
  displayed as stale metadata nor used to choose a similarly named remote track. Remove operates
  atomically on exact durable occurrence IDs, so repeated media can be removed independently and a
  failed mutation changes nothing.
  XSPF remains a local-only interchange format: exporting a regular playlist with any remote or
  unresolved occurrence now refuses the whole export with localized copy before touching the
  destination instead of silently emitting only its local subset.
  Playlist queues keep the playlist as `ViewOrigin` while assigning each item to its actual source.
  Local rows retain exact root/file authority; remote stream and artwork resolution revalidate the
  row's transient closed guard before and after adapter work. Refresh, replacement, retirement,
  disconnect, or shutdown therefore denies stale media instead of replaying a cached locator, and
  reconnect restores a row only when the same `SourceId` publishes the same exact `TrackId`.
  Playback-history ownership remains local-only. Remote ratings retain their live read-only or
  unsupported state, and playlist membership grants no mutation authority. Smart playlists and
  XSPF import/export remain local-only. At that PR's delivery boundary mixed-source metadata export
  and Subsonic server-native playlist synchronization were explicitly deferred; the latter's
  current headless coordination state and remaining GTK work are documented above.
- **Ratings now have a durable ownership, capability, and persistence foundation**
  ([#138](https://github.com/jm2/tributary/pull/138)) — A canonical
  rating is one validated whole integer from 1 through 100, while `None` alone means unrated.
  Tracks carry one coherent Writable, ReadOnly, or Unsupported state, and the catalogue seam rejects
  any per-track capability that disagrees with its backend. Migration 12 adds a nullable local
  SQLite integer with independent storage-type/range enforcement, leaves every legacy row unrated,
  rejects interrupted lookalike schemas without the canonical constraint, recognizes the exact
  definition even after later columns are appended, and supports down/up retry. Tributary owns
  ratings only for local-library tracks: `LocalBackend` transactionally sets
  or clears one exact native ID, returns only committed state, treats deletion as a clean no-op,
  and existing-row metadata refresh or a recognized paired watcher rename preserves the value
  without reading or writing embedded tags. An unrecognized remove-plus-add remains a new unrated
  track.
  Subsonic's valid signed 1–5 `userRating` maps read-only to 20-point increments; valid finite
  Jellyfin `UserData.Rating` and Plex `userRating` values in 0–10 map through rounded tenfold values,
  with native zero retained as canonical 1. Missing, malformed, or numerically unrepresentable
  remote values remain read-only unrated without rejecting their response; DAAP, radio, removable,
  external, and unknown sources are Unsupported, and every remote mutation fails closed. XSPF v1
  deliberately omits ratings, ignores rating-like meta/extension input, and playlist import cannot
  modify a matched library rating. The complete contract is in
  [`docs/ratings.md`](docs/ratings.md).
- **Ratings are now editable, sortable, and usable in smart playlists**
  ([#139](https://github.com/jm2/tributary/pull/139)) — The track list exposes one exact integer
  Rating column. Writable local rows open a keyboard-operable localized popover that accepts only
  1–100 and can clear a rating; read-only remotes explicitly show their value or unrated state as
  read-only, while rating-incapable rows show Unavailable and cannot open an editor. Radio-Browser's
  intentionally compact station schema omits Rating with its other track-only metadata. Every
  nonnumeric cell state, editor action, accessibility label, and failure message is localized
  across all 13 shipped catalogs. Existing profiles see the new column once through a versioned
  config migration, while current profiles retain an intentional hidden or reordered choice.
  Local set/clear requests enter the existing GTK-thread FIFO by exact typed `TrackId`, never
  mutate the row optimistically, and are published only after the SQLite transaction commits.
  Committed rows replace only the matching local identity and invalidate active and cached regular
  or smart-playlist projections; deletion races remain clean no-ops. A storage failure logs its
  detail internally with redacted identity metadata, sends only a typed identity to GTK, and shows
  fixed localized feedback.
  Normal shutdown closes rating admission before the same terminal FIFO marker used by playback
  history, so accepted writes drain and later callbacks cannot cross the barrier.
  Column sorting keeps rated values before missing values in both directions, orders
  readable-unrated before unsupported, and resolves equal values by stable `TrackId`. Smart
  playlists add Rating to filters and compound sorting, exact/not/strict greater/less/inclusive
  range predicates, explicit Is Rated and Is Unrated predicates, and Highest/Lowest Rated limit
  selection. Missing readable values satisfy no numeric predicate—including Is Not—and
  unsupported values satisfy neither numeric nor presence predicates. The editor retains invalid
  input, presents localized visible/accessibility feedback, and disables OK instead of clamping or
  saving it; defensively loaded operands outside 1–100, reversed ranges, and noncanonical presence
  placeholders fail closed. Missing values remain last for ascending and descending primary or
  secondary rating sorts, and limits use stable-ID ties. Existing serialized rule ordering, seeded
  defaults, and migration-11 signatures remain unchanged.
- **Local playback history now has a durable contract, schema, and production pipeline** — Local
  track rows gain a nullable UTC epoch-millisecond `last_played` value, while the architecture
  model safely rejects out-of-range stored timestamps. Migration 10 preserves every nonnegative legacy play
  count, repairs negative corruption to zero, never invents a timestamp from file or library dates,
  validates interrupted SQLite upgrades (including SQLite's equivalent nullable `INTEGER`
  spelling while rejecting incompatible shapes), and supports down/up retry. A pure per-occurrence
  state counts observed forward playback at half of a positive duration rounded up and capped at
  four minutes; missing or zero duration uses the four-minute fallback or an unskipped authoritative
  natural end. Duration freezes on the first positive value; seeks, neutral retry/resume anchors,
  and restarts earn no jump credit; only a forward user seek suppresses the early unknown-duration
  end rule; backward replay preserves accumulated listening; and the count signal latches once.
  `PlaybackSession` now owns that latch per exact local queue occurrence, separately from replaceable
  output-event generations. Only a successfully accepted local delivery—including one reached
  through a local regular or smart playlist—can earn credit. Rejected/stale events earn none;
  pause, buffering, retry, resume, seek, and the three-second Previous restart re-anchor without
  jump credit, while explicit Paused/Stopped state keeps later position polls inert until Playing
  and successful navigation or Repeat One creates a fresh occurrence. Repeat One snapshots only
  the prior history occurrence: a failure before output-generation handoff restores that occurrence
  without cloning or rewinding the queue, shuffle, resolution, or event state, while a generation
  change can never be rolled back. Current output replacement ends playback. The latch closes
  before synchronous FIFO enqueue, preventing duplicate ticks from
  enqueueing another write. Normal shutdown synchronously closes one shared GTK-thread admission
  gate before appending its FIFO marker, disables playback, media-key, seek, open-file, history,
  and root-trust producers, revokes event ownership, stops the output, and waits for every earlier
  admitted history/root-trust command; no callback can queue work behind the marker. That durable
  drain may keep the disabled window visible while an earlier serialized initial or root-trust scan
  finishes. The library engine then atomically updates only that stable `TrackId`, repairs a
  legacy-negative count to one, saturates `play_count` at `i32::MAX`, keeps
  `max(existing, event_timestamp)`, and treats a
  concurrently deleted row as a clean no-op. Only a committed update publishes its replacement
  row; the live Plays value refreshes by stable identity and active/cached playlist projections are
  invalidated without URI matching or phantom rows. AirPlay 1's dedicated RAOP pipeline now samples
  position and duration on a 500 ms timer while Playing and emits generation-scoped evidence when
  available, bringing it under the same accounting boundary as the other outputs. The seeded
  history consumers now use this same committed history through the deterministic contract below.
- **Recently Played and Top 25 now reflect authoritative history deterministically** — One clock
  snapshot governs each smart-playlist evaluation. Recently Played includes only representable,
  non-future `last_played` instants in the inclusive preceding 14 days, orders newest first, and
  breaks timestamp ties by stable `TrackId`; null, corrupt, legacy-unknown, and out-of-window values
  never turn an empty history into a match-all playlist. Top 25 includes only positive counts,
  selects and presents by descending count, then descending last-played with unknown values last,
  then stable `TrackId`, and caps membership at 25. A legacy positive count with no timestamp
  remains eligible for Top 25. Committed history events already invalidate every cached playlist
  projection, reject pre-commit asynchronous results by navigation generation, and immediately
  reload an active playlist, so membership and ordering update without a restart. Fresh defaults
  persist those canonical rules; migration 11 atomically rewrites only byte-exact untouched
  Tributary defaults from both the released v0.5.0 JSON shape and its no-field successor, including
  exact smart/live/match/limit compatibility columns. Renamed, edited, reformatted, non-smart, or
  otherwise divergent playlists remain byte-for-byte user-owned, and interrupted migration is
  rollback- and retry-safe. The smart-playlist editor now exposes Last Played filtering/sorting and
  Most/Least Recently Played limit selection, and preserves authorable Days, Weeks, or Months when
  a relative date rule is reopened and saved.
  ([#137](https://github.com/jm2/tributary/pull/137))

### Fixed
- **Linux library reads no longer feed a recursive rescan loop** — Filesystem notifications
  explicitly classified as access or access-time observations and produced by Tributary's own
  metadata reads are discarded in the watcher callback, before they can consume the bounded event
  queue and falsely request another full-library reconciliation. The filter does not discard real
  create, write, rename, remove, or backend-error events; if accepted events fill the bounded queue,
  overflow evidence remains retained and still schedules an authoritative reconciliation instead
  of being drained or cleared. The persistent unparseable-file cache from
  the original proposal is intentionally excluded: a transient parser or I/O failure can be retried
  by a later scan rather than becoming a stale negative result.
  ([#103](https://github.com/jm2/tributary/pull/103))
- **Unsupported playlist additions now fail visibly and atomically** — Choosing Add to Playlist
  from an authenticated remote, internet-radio, removable-media, or unknown/pathless source now
  shows a localized explanation before any runtime task, database connection, or playlist write.
  Tributary therefore adds nothing instead of silently skipping unsupported rows or modifying only
  an unexpected subset. The existing Add to Playlist, Remove from Playlist, and Properties context
  labels now use their shipped translations as well, and the refusal copy is covered across all 13
  locale catalogs. That refusal remains the fail-closed result for unsupported or unavailable
  selections; the source-scoped storage, default-deny authority, and capability-gated
  authenticated mixed-source interaction are now described above.
- **Shuffled Previous and Next now follow a bounded real playback timeline** — Tributary retains
  the current queue occurrence plus ten actual predecessors, walks backward without fabricating a
  random track at the oldest boundary, and replays fixed forward history before drawing again.
  Duplicate tracks remain distinct queue occurrences; one- and two-item queues, repeat Off/All/One,
  navigation rollback, and queue/output/source lifecycle boundaries are explicitly regressed.
  Repeat All now starts complete queue-occurrence cycles and avoids immediately repeating the
  rollover track instead of omitting that occurrence from the new cycle. Either shuffle toggle
  starts a fresh traversal without changing the current item. The header button and operating-system
  media controls also share one Previous dispatcher: positions above three seconds restart, exactly
  three seconds still navigates, and a retained-boundary Previous restarts the current item.

## [0.5.1] — 2026-07-18

### Added
- **An audited implementation roadmap and clean active backlog** — `docs/roadmap.md` now separates
  product work from the completed holistic-review remediation and classifies all 11 open GitHub
  issues, their current implementation state, dependencies, and likely delivery shape. The active
  `docs/task.md` is reset to 35 countable feature records ordered by correctness foundations,
  bounded user-facing enhancements, and larger data-movement epics. Its first record captures
  hardening for the existing occurrence-aware shuffled Previous/Next history—including bounded
  Repeat All growth, retained-boundary behavior, real UI/media-control path regressions, forward
  traversal after Previous, duplicates, repeat modes, and queue/session resets—rather than
  claiming the feature is absent. The former 220/223 remediation tracker remains intact as
  `docs/task-remediation-2026-07.md`, with its three hardware/installed/live validation records
  explicitly excluded from the new feature percentage.
- **Remote-service tests share bounded protocol-appropriate fixtures and a completed behavior matrix** — Subsonic's ad hoc server is replaced with a reusable loopback service whose method, exact path, and decoded query-subset routes are independent of arrival order. It records bounded request bodies and headers, queues exact response counts, can delay a response body under test-owned deadlines, rejects unexpected or ambiguous requests, verifies every expectation at explicit bounded shutdown, and aborts safely if a test unwinds. Complete production paths cover Subsonic token-query derivation and partial artist/album failure, Jellyfin and Plex authenticated multi-page catalogue loading, Plex atomic per-section failure, Radio-Browser filtering and public redirects, and the geolocation provider cascade; DAAP retains its separately appropriate endpoint-scripted raw-socket fixture for malformed containers and session expiration. The representative matrix covers rejected authentication, credential-safe redirect behavior, finite deadlines, streaming body caps, pagination, partial failure, and root/trailing-slash/escaped reverse-proxy bases without claiming every redundant Cartesian pairing. Twelve additional regressions bring the completion branch to 787 debug and 787 release tests plus strict Clippy in both profiles.
- **Track actions and playback sliders are now accessible without a pointer** — With focus in the
  track list, either the unmodified platform Context Menu key or Shift+F10 opens the same
  selection-snapshotted Add to Playlist, Remove from Playlist, and Properties actions as
  right-click. When no row is selected, the keyboard invocation propagates instead of swallowing
  the key. The list advertises its popup and standard `Shift+F10 ContextMenu` shortcuts to assistive
  technology, and playback position and volume now have distinct localized accessible names in all
  13 catalogs while retaining GTK's native slider role, range, current-value, and keyboard
  behavior. Each one-shot popover owns its action group, so closing it releases the captured
  selection instead of leaving stale menu actions attached to the long-lived track list.
- **Explicit library-root trust and re-enrollment** — Tributary now asks before a legacy or replaced library root can become authoritative, and applies stronger confirmation when a trust request's complete observation has no supported audio files, instead of inferring identity from similar content. A brand-new writable root auto-enrolls only when its first complete observation contains supported audio and it has no remembered metadata. If Tributary has already recorded an empty observation, later content still requires consent because it may be a removable or network volume newly appearing at the mountpoint.
  - The main window automatically queues one prompt at a time for exact configured roots inherited from a pre-identity database, confirmed roots whose identity could no longer be verified, and trust requests whose complete observation has no supported audio files. No Preferences detour is required.
  - Replacement confirmation is presented as destructive. Every prompted no-supported-audio observation, including an empty replacement, requires a separate second acknowledgement because an empty mountpoint is indistinguishable from an intentionally empty library by content alone.
  - Closing, deferring, or giving an unknown response grants no trust. Remembered metadata remains protected until the engine independently revalidates the exact path, private request evidence, persisted state, marker, and mount observation.
- **Identity-preserving library-folder reauthorization** — A library inherited from an older direct host path can now be reselected through the file-chooser portal without minting replacement track IDs. Each Preferences row has a **Reauthorize** action that asks the user to confirm the selected folder is the same logical library, records an immutable OLD→NEW intent while retaining OLD, locks that root against conflicting edits, and completes on restart before normal scanning begins.
  - The destination must be a native absolute path with a distinct, non-overlapping scope. Confirmed roots require the same durable marker; an unconfirmed markerless writable destination receives a marker but still goes through the normal root-trust prompt before its contents become authoritative. Confirmed legacy roots without a marker and markerless read-only destinations remain protected rather than being guessed from content.
  - Track paths and imported-playlist path evidence move in one guarded SQLite transaction while track UUIDs, metadata, timestamps, play counts, and playlist foreign keys remain unchanged. The selected root and marker are retained and revalidated immediately before commit.
  - A completion receipt is committed with the relocation, so an ambiguous database result or crash before config cleanup retries idempotently. Receipt state takes precedence over stale config; a malformed intent without a matching consistent receipt, conflicting scope, receipt-query failure, or inconsistent database state disables both endpoint scopes instead of scanning the wrong path and creating duplicate identities. Config cleanup uses an exact compare-and-swap and an atomic file replacement.

### Security
- **Authenticated requests are pinned to an exact origin** — Every HTTP client that carries a credential (Subsonic, Jellyfin, Plex, DAAP, authentication, artwork, and protected local/AirPlay media fetching) now refuses to follow a redirect to a different scheme, host, or port, and never sends a `Referer`. Previously a redirect could walk a credential to an attacker-controlled host or downgrade it to plaintext HTTP.
- **Credentials are stripped from errors and logs** — Request URLs are removed before an HTTP error is retained or displayed, URL user-info and backend query credentials are rejected or redacted, DAAP session IDs are no longer logged, and MPD command arguments are no longer written to the log. Failures involving MPD server replies or authenticated media URLs, including local and AirPlay pipeline setup/playback, are reduced to opaque categories before they can reach player events or diagnostics. MPD ACK handling validates the single-command index and expected command echo, then retains only a closed typed numeric error category; the daemon-controlled index, echo, and free-form trailing message never enter retained failures, UI text, or logs.
- **Backend credentials no longer enter track or UI state** — Subsonic, Jellyfin, Plex, and DAAP
  catalogue models retain only stable application identity and non-secret metadata;
  backend-native stream/artwork locators,
  authentication, routes, random media leases, and DAAP session keys remain inside the live
  registry-owned adapter. Keeping artwork locators track-only also prevents a Subsonic album or
  artist with the same type-local native ID from overwriting a song's cover. GTK rows and playback
  queues now carry only pathless `SourceId`, exact bounded `TrackId`, and the non-secret session
  epoch that published the catalogue—never a server address, token, salt, username, password,
  authenticated URI, or random lease key. Playback and artwork require that exact epoch before the
  adapter is invoked and recheck the current adapter, lease, and epoch after asynchronous lookup,
  so replacement, disconnect, discovery-route loss, deletion, or shutdown cannot retarget an old
  row through a newer login. Retiring a source stops and clears playback only when that source owns
  the queue; Pause during a pending resolution cancels the completion but leaves Play retryable,
  and a protected-output error forces the next Play through fresh registry resolution. Manual,
  saved, environment-configured, and discovered remote-server URLs are validated before
  persistence, auth-dialog display, logs, discovery/UI publication, or connection ownership; raw
  Jellyfin UDP response bodies are never logged, and malformed input, userinfo, query, and fragment
  state produce one fixed error that cannot echo a rejected secret.
- **Removable-media rows and queues no longer retain mount paths or `file://` locators** — An
  eligible mount's logical GIO key now claims its deterministic `SourceId` into `SourceRegistry`,
  while the current native mount path remains private construction input. The accepted catalogue,
  GTK rows, caches, playback queue, and artwork requests retain only that source ID, a frozen
  lossless native mount-relative `TrackId`, and the exact non-secret publishing epoch. The adapter
  acquires retained mounted-root authority on the blocking runtime, walks deterministically without
  following links or crossing filesystems, and parses bounded metadata through exact already-open
  file handles. Playback and embedded art resolve only IDs present in the exact accepted catalogue,
  recheck the live epoch and revocable media lease, revalidate the root, ancestor chain, and final
  file, and receive one inseparable retained file capability. Only the private filesystem authority
  decodes the relative identity; UI and output consumers never receive or reconstruct a host path
  or direct URI. Pre-unmount, relocation, confirmed removal, replacement, and shutdown disconnect that
  authority before invalidating cache, navigation, playback, or rows. A failed unmount may reconnect
  only from fresh inventory under a new epoch, and confirmed removal releases the provenance claim.
  Pathless removable rows deliberately omit Properties until a separate typed mutation capability
  exists. Unix authority strips only semantically redundant trailing slash and `/.` spellings before
  its no-follow root open, closing a symlink-root bypass. On Windows, traversal temporarily pins the
  exact root and ancestor namespace without delete sharing until the final no-follow file has been
  retained, rejects directory symlinks, and follows a final reparse root only after Windows verifies
  it as a volume mount point; short-lived guards are then released so ordinary device eject is not
  held open unnecessarily.
- **Radio stream locators no longer enter GTK rows or playback queues** — Radio-Browser is one
  stateless built-in `SourceRegistry` session whose Top Clicked, Top Voted, and Near Me feeds are
  independently cancellable views. Accepted snapshots expose only pathless tracks and retain the
  validated station-ID-to-public-URL map privately. If multiple views contribute the same station,
  playback selects the greatest accepted source-wide generation. The resolved request remains
  provisional until the output handoff: weak registry authority plus source and per-view leases
  recheck that exact winner, so view replacement/removal, a newer overlapping view, source
  disconnect/replacement, or dropping the last registry handle fails closed instead of replaying or
  retargeting an obsolete locator. A pending request cannot itself keep that authority alive.
- **Playback-time requests isolate protocol credentials** — Resolved requests are typed, non-serializable, and deliberately omit `Debug`. Their inspectable HTTP(S) endpoint rejects embedded credentials; Plex's token is held as a sensitive `X-Plex-Token` header and Jellyfin's as a sensitive `X-Emby-Authorization` header. Subsonic's `u` plus `t`/`s` or HTTPS-only legacy `p` values and DAAP's bearer `session-id` remain separate private query material and are appended only inside Tributary's app-owned proxy immediately before the exact-origin upstream fetch. DAAP's non-secret `Accept`, `User-Agent`, client-version, and access-index requirements now cross the same typed boundary through a separate exact-name allowlist shared by stream and artwork fetching; receiver, authentication, routing, proxy, framing, and arbitrary headers cannot enter that channel. A DAAP request also carries a revocable live-session lease, so replacement, release, discovery loss, disconnect, or shutdown invalidates an already-issued request. The proxy accepts only allowlisted trusted and authentication fields, installs request-owned values before the receiver's `Range`, forwards no other receiver header, and every output's typed load path fails closed instead of falling through to the clean endpoint.
- **HTTP responses are bounded while streaming** — API, authentication, DAAP, radio, artwork, and metadata reads now count bytes as they arrive and stop at an endpoint-specific cap, with an end-to-end deadline in addition to the idle timeout. A hostile or broken server previously could exhaust memory by lying about `Content-Length` or by never ending a chunked body.
- **Chromecast frame lengths are bounded before allocation** — `rust_cast 0.21` otherwise reads a peer's unsigned 32-bit Cast length and immediately reserves that many bytes. Tributary now wraps the decrypted worker-local stream with a framing guard that buffers the complete big-endian header and rejects control-message payloads above a deliberately generous 1 MiB before the upstream message manager sees the header or allocates. Accepted headers and payloads remain byte-for-byte unchanged, writes pass through unchanged, and an early EOF inside either the header or advertised payload fails closed and retires the poisoned session. Regressions through the real upstream manager cover oversized rejection without a payload read, an exactly-at-limit protobuf message, fragmented/truncated input, and consecutive frame reset.
- **Third-party requests refuse plaintext downgrades** — Radio-Browser, IP geolocation, and MusicBrainz requests still follow the cross-host redirects those services depend on, but no longer follow one from HTTPS down to HTTP, and no longer send a `Referer`.
- **Flatpak filesystem access is consent-based and narrowly scoped** — The sandbox no longer exposes the user's entire home directory, even read-only. XDG Music remains read/write; automatic Devices entries receive read-only file access only under the standard host mount roots `/media`, `/run/media`, and `/mnt`. The reviewed `org.gtk.vfs.*` session-bus namespace exposes host GVfs service methods for GIO's native mount inventory, which can still list an eligible inaccessible root elsewhere, while Tributary omits raw USB/all-device, UDisks, whole-host, and non-native GVfs filesystem grants. Selecting a custom library through Preferences requests a persistent read/write file-chooser portal grant, subject to ordinary host filesystem permissions. A fail-closed CI allowlist and adversarial fixtures reject every unreviewed Flatpak finish argument.
- **Dependency-audit findings are resolved or explicitly time-bounded** — The yanked `spin 0.9.8` transitive dependency is updated to compatible `0.9.9`. The two remaining warnings (`paste` and `proc-macro-error2`, both unmaintained) have documented dependency paths, follow-ups, and review deadlines. The sole ignored vulnerability, `RUSTSEC-2023-0071` for `rsa`, exists only in `Cargo.lock` through inactive MySQL support: Tributary enables SQLite, but `cargo-audit` checks locked optional packages. Because no fixed release exists, the ignore remains until 2026-10-10 or the next release, whichever comes first, with immediate review if MySQL support is enabled.
- **Chromecast no longer receives your server credentials** — Casting a track from Subsonic, Jellyfin, or Plex used to hand the Chromecast the stream URL with your credential still in it: your Plex token, your Jellyfin API key, or — with Subsonic's plaintext auth mode — your **actual password**, hex-encoded and trivially reversible. That credential went to a device Tributary does not control, over a LAN it does not control, and was retained in the device's media session. Tributary now resolves the live source inside the app, fetches the stream itself, and gives the device an opaque, revocable, single-media ticket; even a clean-looking resolved endpoint must take the typed proxy path. A new load (including direct or invalid media), user Stop, source-lease revocation, or output/server teardown retires the old route; pause and seek keep it available for byte-range refetches only within its hard lifetime. Internet radio still plays directly after its credential-free locator is resolved and revalidated at use, but that locator no longer lives in GTK or the queue.
- **MPD no longer receives or stores backend credentials** — Remote playback is resolved from an opaque live-source reference into an app-private typed request before MPD is invoked; the legacy/direct URI classifier remains a fail-closed defense for DAAP and other supported protected inputs. After connecting, Tributary binds a dedicated proxy to the successful socket's local IPv4/IPv6 address and gives plaintext `addid` only a bracket-correct opaque ticket, so neither the endpoint nor credential crosses the MPD connection or lands in daemon queue state and logs. The protected upstream is fixed at registration, fetched by Tributary's no-`Referer` exact-origin client, and accepts only the receiver's `Range` header. Missing runtime or connection-address state, unusable scoped/link-local IPv6, bind/registration failure, malformed HTTP(S), unsupported credential schemes, an inactive source lease, and invalid generated arguments all fail closed. Tickets survive pause, seek, and a restartable remote Stop within their hard lifetime, but are revoked sooner on any replacement load, user Stop, source replacement/release, output drop, natural end, ownership loss, operation failure, worker shutdown, or stale generation; stale cleanup cannot revoke a newer ticket. Upstream body failures are reduced to URL-free diagnostics.
- **Local and AirPlay GStreamer no longer receive backend credentials** — Before `playbin3` or AirPlay's `uridecodebin` can consume protected media, Tributary exchanges the playback-time typed request for an opaque ticket on a dedicated loopback-only server. The app-owned proxy fixes the upstream target, applies the exact-origin/no-`Referer` policy, and forwards only `Range`; credential-free radio, files, and library paths remain byte-for-byte direct. Malformed or unsupported protected media and missing runtime, bind, client, ticket, or active source-lease state fail closed with fixed URL-free events. Replacement by any direct, protected, or rejected load, Stop, source replacement/release, EOS/error, setup/preroll/start failure, output teardown, and proxy teardown revoke the route; pause, play, and seek retain it only within its hard 24-hour lifetime. Each load owns its server and cleanup is identity-checked, so a stale pipeline callback cannot revoke a newer ticket. Server binding and revocation run outside the short-held proxy-state mutex; generation ownership lets a newer load, Stop, or runtime replacement supersede an in-flight startup without waiting and prevents that older startup from installing afterward.
- **Protected loopback tickets cannot escape through an ambient HTTP proxy** — GStreamer's `souphttpsrc` may consult process or operating-system proxy settings even for `127.0.0.1`; an empty proxy property still restores the default resolver under libsoup3. Local and AirPlay source-setup now recognizes only Tributary's exact HTTP loopback `/cast/<UUID[.extension]>` shape with an explicit port and no user-info, query, or fragment, installs the `direct://` resolver, disables retries, verifies its timeout/policy round trip, and posts a fixed error while locking the source in NULL if any property cannot be enforced. Ordinary files and internet radio retain their existing proxy behavior, while the app-owned upstream request may still use a legitimate configured proxy. An isolated child-process regression receives poisoned proxy variables before process creation, keeps the parent-owned proxy listener live through child completion, and starts the child-owned media listener's bounded window only after cold GStreamer plugin discovery, then proves the media fixture receives the ticket and the ambient proxy receives nothing.
- **Protected DAAP and Subsonic playback now has a real-GStreamer regression** — One bounded child process constructs production backend-shaped requests and sends both sequentially through the real local `Player`, per-load app-owned proxy, `souphttpsrc`, FLAC decoder, and `fakesink` to a decoded-buffer handoff and generation-owned end-of-stream. The fixture proves exact upstream paths, private queries, DAAP protocol headers, absence of `Authorization`, `Proxy-Authorization`, `Cookie`, and `Referer` at the upstream fixture, opaque ticket containment, and source-setup's direct/zero-retry/30-second policy. Missing plugins, request drift, player errors, absent decoded audio/EOS, and native hangs fail rather than silently skipping. This regression runs against each build host's GStreamer installation; the packaged-Windows proof below independently covers the bundled plugin and DLL set.
- **Windows packages prove protected playback uses only their bundled GStreamer runtime** — Before the portable ZIP is created, the finished distribution now runs its own hidden, headless probe from a fresh external registry with ambient GStreamer paths, proxy-resolver overrides, HTTP proxy variables, and MSYS2 DLL search paths removed. A bounded, deduplicated closure seeded by the app, scanner, and every plugin uses the selected architecture's absolute `llvm-readobj.exe` to inspect PE imports as data rather than executing arbitrary plugins through `ldd`; newly copied MSYS2 runtimes form the next exact closure round, with no broad `bin` sweep. The Soup plugin is inspected alone first so its direct `libsoup` edge must be observed, copied, and inspected. Remaining targets run in batches of at most 28 under command-line, output, per-process, process-tree, and five-minute whole-closure limits, with fixed-size failure diagnostics and Windows PowerShell 5.1-compatible process handling. This replaces an ARM64 `ldd` path that first missed path-only records and then hung for more than 33 minutes while executing `libgstencoding.dll`. The package places its exact-architecture `gst-plugin-scanner.exe` beside Tributary's root-level DLLs and executes that exact helper under a five-second deadline before GStreamer can fall back to in-process discovery; the probe's PATH contains only System32, so it receives no DLL-search assistance unavailable to an ordinary user launch. Every required factory and the selected FLAC decoder must resolve beneath the distribution's plugin directory. The probe sends an embedded FLAC through the production protected-loopback callback to a decoded buffer and EOS, verifies `souphttpsrc` retained direct routing, zero retries, and the 30-second timeout, proves a poisoned proxy received no connection, and proves an alternate source is locked in NULL with the fixed fail-closed error. Listener deadlines arm after cold factory discovery. Before transitioning GStreamer to NULL, the probe publishes a listener cancellation phase while both listeners remain accepting; afterward it publishes a separate final stop, drains and counts queued accepts until the nonblocking listener reports an empty queue, then joins and inspects. Only an already-accepted media request that ends with incomplete-header EOF, connection-aborted, or connection-reset I/O in the cancellation phase is cancellation. Malformed UTF-8, request-line, route, method, header, range, timeout, other I/O, accept, and response-write failures remain fatal even when teardown overlaps, while the poison observer covers the complete NULL transition. Deterministic server tests pin the production ordering, poison observation window, queued-accept drain, and semantic-failure boundary. A missing required DLL, plugin, or scanner, external provenance, a stale cache, request/policy drift, absent decode/EOS, crossing the 1 MiB output-flood threshold, native hangs, or a missing exact success sentinel fails packaging under independent Rust and 90-second process deadlines; captured diagnostics are fixed-size tails. Process-tree termination and exact argument passing feature-detect newer .NET APIs with bounded Windows PowerShell 5.1 fallbacks. CI and release workflows invoke the same pre-archive script on native x86_64 and ARM64 runners. After PR #124's named arguments passed both jobs but the affected host reproduced the same singleton-target failure, PR #127 replaces the function-boundary array with explicit `List[string]` singleton, round, and batch values and reports the exact failed target predicate with bounded single-line diagnostics. Native x86_64 and ARM64 package CI, including the Desktop PowerShell 5.1 path, passed in run `29648906031`; the exact affected-host rerun and live packaged-Windows DAAP/Subsonic playback remain separate manual checks.
- **Credential-bearing media tickets now expire** — Every upstream proxy ticket has a hard, absolute, non-sliding 24-hour lifetime from registration in addition to earlier playback-lifecycle revocation. It is usable only before its monotonic deadline; a lookup at or after that boundary atomically removes it and returns the same 404 as an unknown or revoked route. GET and byte-range requests, pause, seek, and receiver status do not renew the deadline, so a compromised receiver cannot perpetuate the bearer. A response admitted before expiry may finish afterward, but every later lookup fails. Local-file routes retain their existing server-lifetime behavior because they front no backend credential.

### Changed
- **Project documentation now describes the shipping source architecture and remaining work** — The
  README no longer presents the implemented P3.1 lifecycle as a future URL-keyed design or labels
  AirPlay 1 as scaffolding. Its architecture diagram, project tree, removable-media lifecycle,
  pathless retained authority, smart-playlist limitations, ratings status, playlist scope, and
  shuffle-history status now match the code. The source-lifecycle ADR links its archived tracker;
  Subsonic, Jellyfin, Plex, and device module docs now describe current protected at-use resolution,
  catalogue publication, and the still-unimplemented transfer/MTP scope without referencing closed
  issue #1.
- **Managed network sources now share one production lifecycle authority** —
  `SourceRegistry` is the sole adapter/session owner for Subsonic, Jellyfin, Plex, DAAP, and the
  built-in Radio-Browser source
  across environment startup, manual and interactive authentication, discovery, initial catalogue
  publication, at-use stream/artwork resolution, disconnect, route loss, deletion, and shutdown.
  The separate standard and DAAP registries and their URL/lease-key UI ownership have been removed.
  GTK consumes one atomic lifecycle baseline through a monotonic invalidation watch and reduces
  exact connection generation, non-secret session epoch, catalogue, closed failure category,
  provenance, visibility, cancellation, and retirement state into the sidebar and browser. An
  accepted catalogue clears its exact pending guard before any row rebind or selection signal. If
  its exact row was already selected but the guarded rebind could not activate it, GTK invalidates
  and reselects that same row once the catalogue is authoritative; stale or superseded catalogues
  cannot activate themselves. Programmatic selection and fallback paths snapshot `RefCell`-backed
  navigation keys and release every borrow before GTK can synchronously re-enter a signal handler.
  A stale failure clears only its own intent; same-epoch catalogue publication preserves playback
  and navigation, while a replacement epoch invalidates old media before rendering the successor.
  Saved, Environment, and Discovery publishers own independent keyed/refcounted claims, so deleting
  Saved demotes a still-discovered row. Withdrawing Discovery revokes active or pending work that
  captured that advertised route and clears the route even when Saved or Environment keeps the
  logical row visible; the reducer still retires its cache, playback, and active projection. Hidden
  or absent lifecycle snapshots synchronously clear pending state, cache, queue, navigation, row,
  and empty category presentation rather than waiting for retirement pruning.
  DAAP server-info/login now returns immediately after parsing `mlid`; the framework stages exact
  close authority before update, database discovery, items, or initial catalogue loading begins.
  Cancellation after session acquisition, malformed post-login catalogue failure, replacement,
  disconnect, discovery loss, and shutdown therefore revoke media and elect one bounded logout
  owner. Interactive Jellyfin `AuthenticateByName` similarly finishes under protected
  construction, synchronously stages its newly minted session token before ping or catalogue work,
  and sends exactly one `Sessions/Logout` from registry-owned retirement. Pre-existing Jellyfin API
  keys and Plex legacy tokens remain explicitly non-owned durable credentials: disconnect revokes
  all local adapter/media authority without attempting broader server-side credential revocation.
  If normal client construction fails after a safely representable interactive token was minted,
  the exact pre-authentication route attempts one bounded best-effort logout. A hostile token with
  control bytes cannot be represented in the exact authorization header; that narrow case fails
  closed without echoing, transforming, or sending the token, so an unsafe logout is deliberately
  impossible. Application shutdown joins admitted retirement work. Menu and Ctrl+Q quit requests
  close the active window instead of bypassing its `close-request` barrier; only an application with
  no window quits directly. The focused lifecycle module passes all 53 tests. Locked debug and
  release suites each pass 20 library, 865 application, and 10 repository-metadata tests (895
  total), with locked all-target/all-feature compile, strict warning-free Clippy, formatting, and
  diff checks green. The CodeQL review follow-up generates all three password-bearing test inputs
  at runtime instead of embedding credential-shaped literals, preserving the exact authentication
  and logout coverage without suppressing hard-coded-secret analysis. Lifecycle, registry,
  reducer, playback-boundary, provenance, shutdown,
  interactive-Jellyfin, and actual-wire DAAP regressions cover the authenticated cutover.
  Radio-Browser now uses three exact independently cancellable views, preserves a predecessor on
  failed refresh, publishes accepted empty results authoritatively, and resolves public streams
  from private locator contributions only at use. Near Me performs translated consent in GTK; an
  exact generation-owned prerequisite marker prevents unrelated lifecycle invalidations from
  treating the deliberate pre-construction dialog interval as source loss, while a stale or
  superseded dialog cannot suppress ordinary fallback. Automatic construction failure or later
  source loss now returns a selected radio lane to Local with the user's music-column names,
  column visibility, and browser visibility restored. The adapter tolerates partial successful
  tiers, deduplicates by tier precedence, computes each retained station's distance once, and then
  applies one stable global distance sort.
  Locked all-target/all-feature check, strict debug/release Clippy, formatting, and diff checks
  pass; complete locked debug and release suites each pass 20 library, 895 application, and 10
  repository-metadata tests (925 total). Independent integrated review found the consent race and
  the automatic-fallback presentation leak; both are fixed.
  The OS-opened external-file adapter described below closed one remaining boundary. The removable
  adapter now closes the last one with registry-owned mounted-root authority, pathless epoch-bound
  catalogues, and retained-file resolution, completing P3.1's implementation record.
- **OS-opened files now use an ephemeral registry-owned lifecycle instead of direct path queues** —
  A delivery is processed sequentially on a blocking worker, preserving candidate order, skipping
  files that cannot be opened, parsed as audio, or accepted by the metadata bounds, and stopping
  after the first accepted candidate. A native non-UTF-8 leaf name becomes bounded lossy Unicode
  only as a parser/presentation hint; the already-open handle remains the sole authority. Before
  identity or adapter publication,
  the worker must still own the delivery's exact admission generation under the shared
  shutdown/publication gate. A newer delivery, every explicit Play/Pause/Next/Previous/scrub action,
  Stop, a real output change, or shutdown supersedes older admission; selecting the already-active
  output remains inert. Only a successfully parsed already-open regular file receives fresh random
  source and track IDs. Its one-item queue carries those pathless IDs and the exact registry epoch,
  while the hidden adapter retains the original handle behind a `MediaLease` checked both before and
  after every file-handle clone. The closed stream resolver now returns either `Http` or a retained
  `File` capability, so remote/radio behavior is unchanged and external playback never reconstructs
  a URI. Cursor-based tag and artwork parsers serialize per retained capability, while every output
  continues to consume the app-owned proxy's position-independent reads.
  Embedded art receives a cloned capability only after the selected output accepts the load.
  Replacement by another external or ordinary queue, Stop, unrepeated EOS, playback/load failure,
  real output change, stale post-adoption admission, and shutdown explicitly retire the ephemeral
  source idempotently. Registry shutdown and adoption serialize under the same gate; lifecycle
  baselines clear hidden state only for real UI owners, so the intentionally hidden source cannot
  invalidate its own playback. The OS-open callback now logs only a count and fixed status, never a
  delivered path. Focused regressions cover ordering, exact admission and shutdown races, random
  identity and epoch isolation, retained-handle path replacement, lease revocation,
  hidden-baseline ownership, and exactly-once retirement; independent review covers the complete
  intent/terminal wiring and post-accept artwork handoff. Locked debug and release suites each pass
  940 tests, strict Clippy is clean in both profiles, and final independent integrated review is
  clean.
- **Removable mounts now use the shared source lifecycle from discovery through playback** — The
  window-owned GIO controller still performs only cached inventory work on GTK's main thread, but
  it now owns keyed provenance claims and delegates scanning, catalogue publication, media
  resolution, cancellation, disconnect, and shutdown to `SourceRegistry`. Stable `SourceId` text,
  rather than the transient logical inventory key or current mount path, keys navigation and cache
  state. Selecting an accepted row renders its pathless catalogue; relocation or pre-unmount
  disconnects the exact source before invalidating cache and source-owned playback, a cancelled
  unmount reconnects only through a fresh inventory observation and new epoch, and confirmed removal
  releases the claim after retirement. Deterministic lifecycle tests cover accepted publication,
  unlisted-track rejection, stale lease and epoch rejection, reconnect, queued-scan cancellation,
  shutdown joining, and replay of a retained background scan failure when its inactive row is later
  selected. Together with mounted-authority, identity, adapter, and GTK coverage, the complete
  serial locked debug suite passes 20 library, 926 application, and 10 metadata tests (956 total).
  Locked all-target/all-feature check, strict debug Clippy, formatting, and diff checks are
  green. A release-suite attempt was interrupted when partial artifacts exhausted temporary
  workspace disk quota; no release validation result is claimed from that environmental failure.
  This completes P3 at 30/30 and advances the remediation tracker to 220/223 (98.7%); only three
  manual P2 field validations remain, while the release-workflow run remains deliberately deferred.
- **The centralized source-lifecycle foundation now backs the production source registry** —
  `SourceLifecycleRegistry` provides the atomic adapter, revocable media lease, session epoch,
  accepted catalogue/view snapshots, keyed/refcounted provenance, and generation-correlated
  sanitized failures used by `SourceRegistry`. Framework-owned adapter wrapping and an
  unforgeable close capability send rejected, stale, cancelled, panicking, replaced, disconnected,
  and shutdown sessions through one exactly-once tracked retirement path. Each spawned connection
  generation retains a settlement participant through construction and any late rejected-adapter
  close. One composite disconnect waiter joins the adopted retirement, a dissociated predecessor,
  and every still-unsettled generation—including superseded generations—and returns a sanitized
  late close failure only after all joined work settles. Participant ownership transfers before the
  constructor task can drop, so there is no false zero-count completion window. Final provenance
  release also retires and prunes a settlement-only disconnect without starting another close.
  Reconnect can adopt a successor while its predecessor closes, and at-use HTTP resolution checks
  the expected epoch before adapter access, then rechecks the adapter, lease, and epoch after
  asynchronous lookup. Protected construction, atomic task admission, persistent shutdown,
  last-handle fail-closed cleanup, lifecycle-owned pruning, and coherent baseline/watch observation
  retain their deterministic adversarial coverage. This was introduced as an unwired foundation
  earlier in the release cycle; the production cutover above now consumes it instead of maintaining
  a second lifecycle owner.
- **Playback queues and output selection now share tested lifecycle boundaries** — Starting from
  the track list captures its current sorted/filtered `ListModel` projection exactly once. Initial
  direct playback, Next, Previous, EOS replay, and a retry after synchronous refusal now all hand the
  session's current immutable item to the active output under a fresh event generation, while Stop
  invalidates that ownership before it invokes the backend. Sorting, filtering, or navigating the
  visible projection therefore cannot retarget the playing identity or make a stale output event
  current. Output changes use one validated transaction: selecting the exact active endpoint is a
  complete no-op; a real change clears generation ownership before stopping the displaced output,
  parks and later restores the exact Local instance, and stops/drops replaced remote outputs. A
  wrong-type replacement or inconsistent parking state leaves the current target, queue, and
  output unchanged. The old switch path constructed a throwaway MPD worker solely to move Local
  into its parked slot; that worker is no longer created. Headless recording-output regressions
  exercise reorder/filter/source-navigation, B→C→B queue identity, stale events, rejected remote
  load retry, Local→MPD→Chromecast→Local replacement/restoration, invalid replacement, Stop/drop
  cleanup, and exact reselection through the production seams. Keyboard context-menu matching
  now ignores ambient lock/legacy modifier state such as NumLock while continuing to reject real
  Control, Alt, Super, and extra-Shift chords; owned local and remote artwork buffers also cross
  into GLib without a redundant full-buffer copy. Together with the accessibility,
  Cast-frame, recycled-row, artwork, and source-generation harnesses, this completes all P3.4
  integration items; the final rebased branch passes 797 tests in each profile plus strict Clippy
  in debug and release.
- **Local and remote track catalogues now share the backend abstraction** — `MediaBackend` now
  exposes one complete-track catalogue operation, and the only production publication adapter
  accepts `&dyn MediaBackend`. Scanner snapshots construct `LocalBackend` instead of querying the
  track table directly, while Subsonic, Jellyfin, Plex, DAAP, environment, discovery, and manual
  connection paths use that same adapter rather than backend-specific `all_tracks` methods.
  Authentication and protected-media/session retention remain concrete because those lifecycles
  are intentionally backend-specific; this slice makes catalogue publication the real seam without
  claiming that every source-lifecycle or browsing path has already converged. If a passwordless
  DAAP catalogue read fails after login, Tributary now logs out and emits the same one-shot UI
  failure signal as a connection error, so the sidebar spinner, pending guard, and prior selection
  are restored instead of remaining stuck. Together with the stable local aggregate identity,
  grouping, and by-ID work accepted in PR #114, this completes P3.2's bounded backend-abstraction
  scope; broader source/session lifecycle convergence remains tracked under P3.1.
- **MPD playback now requires explicit exclusive partition control** — MPD exposes pause, stop,
  and its `repeat`, `random`, `single`, and `consume` settings as partition-wide mutations, with no
  atomic command that can prove Tributary still owns the current song. The Add Output dialog now
  explains that scope in every supported locale and requires confirmation that no other controller
  or Tributary instance shares the playback partition. The confirmation is persisted as an
  explicit mode and participates in output identity, so reselecting an upgraded active endpoint
  rebuilds it instead of being mistaken for a no-op. Legacy entries deserialize unconfirmed and
  every load intent fails with localized re-add/confirm/reselect guidance before optimistic
  Buffering state, output-epoch advancement, worker enqueue/cleanup, MPD connection, MPD state or
  option commands, protected-media tickets, or queue mutation—even when media was already rejected
  as malformed, unsupported, or inactive. The synchronous refusal leaves the exact queue item
  retryable, so another Play re-shows the guidance instead of toggling an empty MPD session; an
  independent worker gate retains the same fail-closed contract for internal callers.
  Re-adding the exact host and port with the checkbox upgrades that entry in place without renaming
  it, dropping siblings, or creating a duplicate. Existing song-ID ownership checks remain in
  force: observing a foreign current song violates the exclusive-control promise but still causes
  conservative relinquishment/retention rather than a racy stop or delete.
- **Coverage is now representative and threshold-gated** — The sole comparable report moves to
  a dedicated Linux x86_64 job pinned to Rust 1.92.0, matching LLVM tools, cargo-llvm-cov 0.8.7,
  the committed dependency graph, every host target, and every feature. UI, Jellyfin, Plex,
  Subsonic, radio, migrations, desktop integration, and `main.rs` are no longer excluded from CI
  or any developer helper. A reviewed `coverage-baseline.txt` enforces a 66.9% line floor and the
  HTML report uploads even when the threshold fails, and a missing artifact is itself an error.
  Two exact pinned reports measured 67.03% and 67.02% lines, correcting the provisional local
  estimate and confirming the floor under the documented two-run policy; CI prints that measured
  summary even on a threshold failure. Two repository contract tests prevent the toolchain,
  source set, threshold wiring, artifact policy, summary, and local commands from silently
  drifting. CI enforces the checked-in floor but does not compare it with the base branch; README
  records the repository's non-decrease review policy and explains why developer-helper and
  native-platform summaries remain informational rather than directly comparable to the canonical
  denominator.
- **Sources and playback queues now use stable, location-independent media identity** — The
  accepted P3.1 contract is now backed by frozen `SourceId`, exact bounded backend-native
  `TrackId`, composite `MediaKey`, and separate playlist/radio `ViewOrigin` types. Saved remotes
  added without an existing live row receive random persisted source IDs; legacy, discovered,
  environment, and unsaved endpoints use deterministic backend-plus-canonical-base-URL IDs, and
  promotion persists that already-live deterministic ID. Local, Radio-Browser, and removable
  logical sources use pinned deterministic identities. Those typed IDs now travel through sidebar
  rows, standard and DAAP connection ownership, sync events, navigation, disconnect, discovery
  loss, deletion, and playback instead of using a configured URL as generic ownership. Subsonic song
  IDs, Jellyfin item IDs, Plex rating keys, and decimal DAAP item IDs remain exact and
  source-scoped rather than being irreversibly projected into global UUIDs. Empty or oversized
  provider IDs fail closed and debug output retains only their byte length. Production catalogue
  fixtures pin exact surviving values (`healthy-track`, `track-0`, and DAAP `9`), while frozen
  removable-source and native-path goldens cover mount relocation and unpaired Windows UTF-16 code
  units.
  The saved-source file is now a strict version-1 envelope containing `source_id`. A legacy bare
  array is validated, canonical duplicates collapse deterministically to the first row, and the
  complete migrated file atomically replaces `servers.json` before any row is published. Unknown
  versions, malformed identities, endpoint/ID conflicts, and failed replacement publish nothing
  and leave the original complete file unchanged for explicit recovery. A version-1 remote accepts
  only a random RFC UUIDv4 identity or the exact UUIDv5 owner of its canonical backend and endpoint;
  other UUID versions and UUIDv5 values owned by another remote, built-in, or removable source
  quarantine the complete file. Repeated manual Add reuses the saved owner,
  discovered-to-saved promotion first persists the row's already-published ID and retains its
  ephemeral advertised route for the immediate route-aware authentication/connection attempt, and
  saved-plus-environment startup uses the stored ID for authentication; all three paths therefore
  keep one canonical `(backend, endpoint)` owner without trying to transfer live UI, cache,
  playback, or registry state or discarding discovery-only reachability. An accepted
  reconnect publication always clears the canonical row's transient connecting state—even when a
  prior session still marks it connected—so repeated Add, promotion, and environment reconnects
  cannot leave the sidebar spinner stuck. A newest environment authentication or catalogue
  failure now carries its exact `SourceId` and opaque UI-attempt token back to GTK and clears only
  that attempt's transient spinner while preserving any prior connected session. An attempt
  already superseded at the registry publishes no failure transition; if a retry starts after an
  older failure is queued, its new token prevents that stale event from clearing the retry.
  Playlist queues now carry local media identity plus their
  playlist origin, overlapping radio feeds share station media identity without sharing view
  ownership, removable tracks use a lossless native mount-relative ID that survives a mount-point
  change, and each OS-opened file session mints independent random source and track IDs. The
  later authenticated-remote lifecycle cutover consolidates the standard and DAAP owners behind
  `SourceRegistry`; the Radio-Browser follow-up makes its queues pathless as well. At that
  identity-only cutover, removable and external queues still retained their current locators. After
  merging the completed P3.4 seams, the accepted
  pre-follow-up head passed 823 tests
  and strict all-target/all-feature Clippy in both profiles. The final identity review adds three
  regressions for the persisted-ID contract and promoted advertised route: the complete debug and
  release suites now pass 827 tests each, the 49-test identity filter is clean, and strict Clippy
  passes in both profiles. The promoted-route regression generates its disposable authentication
  secret at runtime instead of embedding a credential-shaped literal, keeping the security scan
  meaningful without changing production authentication behavior.
  Automated review also removed a redundant copy of each accepted Radio-Browser native ID.
  Remote-reference decoding deliberately continues to reject uppercase hex as noncanonical; an
  existing malformed-reference regression pins that fail-closed boundary. Codex review found the
  failed saved-plus-environment spinner path; its reordered same-source and exact-owner regression
  proves cleanup neither clears a newer retry, disconnects the retained predecessor, nor mutates
  another source row. The failed-migration regression now injects the atomic-replacement error at
  the loader's private persistence boundary instead of relying on directory permissions, which a
  privileged Linux CI container can bypass. Linux, Windows, and unprivileged test runs therefore
  exercise the same fail-closed no-publication/original-bytes contract without changing production
  migration behavior.
- **Source identity and lifecycle now have a recorded architecture contract** — The P3.1 decision
  defines immutable `SourceId` plus backend-native `TrackId` identity, deterministic migration for
  legacy saved sources, one registry-owned connection/refresh/cancellation/failure state machine,
  explicit exactly-once DAAP retirement, and ID-at-use resolution for local, playlist, radio,
  removable, remote, and OS-opened media. It separates playlist and radio-feed view identity from
  underlying media identity and documents the evidence limits of discovered endpoints and
  removable-device keys. The migration contract pins the legacy-array reader, versioned envelope,
  atomic replacement, deterministic duplicate collapse, and fail-closed whole-file conflict
  quarantine. Overlapping radio views retain separate locator contributions and choose the newest
  initiated accepted refresh deterministically rather than completion order.
  PR #113 recorded this decision only. PR #120 implements its stable source/media identity and
  saved-source migration; PR #121 adds exact local/playlist resolution plus retained root/file
  authority through output consumption; and the authenticated-remote production cutover unifies
  Subsonic, Jellyfin, Plex, and DAAP session, refresh/failure, provenance, and GTK projection
  ownership. At that cutover, embedded-art authority and the radio/removable/external at-use
  locator adapters remained implementation work tracked under P3.1. The retained-art and
  Radio-Browser follow-ups close the local/playlist and public-radio boundaries. The subsequent
  external-file adapter closes OS-opened-file admission, retained capability, and retirement. The
  final removable adapter adds the same pathless registry ownership with mounted-root authority and
  closes P3.1's implementation record.
- **Local album and artist aggregates now have stable identities** — `LocalBackend` replaces its
  per-call random album/artist UUIDs with UUIDv5 values under a private versioned namespace. Artist
  and album domains are separate, every component is length-framed to prevent concatenation
  ambiguity, and metadata bytes are intentionally neither case-folded nor Unicode-normalized. A
  local track now carries the same performing-artist and album IDs returned by aggregate listings,
  so an unchanged database produces the same identities across queries and restarts.
  - Albums are keyed by exact title plus effective album artist, rather than title alone. A missing
    or Unicode-whitespace-only album-artist tag falls back to the exact performing artist; nonblank
    tags retain their original case, normalization, and surrounding whitespace. Same-titled albums
    by different artists therefore stay separate, while compilation tracks with one album artist
    group together. Aggregate year and genre use deterministic minima instead of SQLite's arbitrary
    bare-column value; counts, durations, per-artist album totals, and library album statistics use
    the same disambiguated key. `Album.artist_id` remains empty because a compilation credit does
    not necessarily identify a performing-artist entity.
  - `get_album_tracks` and `get_artist_tracks` are no longer unsupported. They first map a UUID to
    its compact deterministic metadata key, return an empty list for an unknown UUID, and then
    issue a deterministically ordered SQLite query narrowed to the exact album title or performing
    artist. Album results retain an exact effective-artist filter so blank-tag fallback cannot
    merge another same-titled album. Identity-bearing metadata edits intentionally create a new
    aggregate identity. The common complete-catalogue backend seam was completed later in this
    development cycle. The P3.1 stable-identity milestone subsequently preserved exact persisted
    local track-ID strings and replaced the random malformed-UUID fallback with a frozen
    deterministic compatibility projection. PR #121 subsequently adds current root/file authority
    and retains it through the complete playback-output lifetime.
- **Minimum supported Rust version raised to 1.92** — Originally raised to 1.85 in this cycle, but the gtk-rs 0.11 release series (gtk4, glib, gstreamer) requires rustc 1.92, so 1.85 had been unbuildable fiction since that upgrade. The declaration now matches reality and a dedicated CI job compile-proves the MSRV against the committed lockfile on every push, so it cannot silently drift again.
- **The local validation gate is now enforced by CI** — Debug-profile `cargo test --all-targets` (release-only testing compiled out `debug_assert!` bodies and overflow checks), fuzz-workspace `fmt`/`clippy` against its committed lockfile (which had already drifted past the supported toolchain unnoticed), strict no-diagnostics `desktop-file-validate`, and `appstreamcli validate` all run on every push and pull request. Repository-level tests also pin the Rust/API floor, native package requirements, desktop launch field, and main category together so those declarations cannot silently diverge again. Their workflow inspection is line-ending agnostic and explicitly exercises a synthesized CRLF checkout, so Windows cannot report a missing job merely because Git converted the YAML file. Windows architecture jobs now finish independently instead of matrix fail-fast cancelling the surviving diagnostic signal, and the ARM runner skips setup-msys2's optional package cache after its `paccache` cleanup intermittently failed even though package installation had succeeded; Cargo retains its separate architecture-specific cache.
- **Flatpak source generation is consistent across local and CI builds** — The exact checksum-pinned `flatpak-cargo-generator` revision is now vendored with provenance, an MIT license notice, and update instructions. One repository-root-independent helper verifies those bytes, Python 3.9+, and the exact direct dependency versions before always generating `build-aux/flatpak/cargo-sources.json`; the Linux build helper and CI both call it instead of downloading the generator or writing the manifest to the wrong directory. The Python transitive dependency graph is not hash-locked. Local instructions keep those packages in an XDG cache virtual environment and configure Flathub. Flatpak-only mode now bypasses native Rust/GTK prerequisite checks, the directory source excludes known VCS/agent/generated state (including host `target/`), the builder installs the runtime, SDK, and Rust extension from Flathub, and the single-file bundle records that runtime repository. The release workflow remains deliberately unchanged for now and continues to download and verify the identical immutable generator revision before use.
- **Foreign keys, WAL, and busy timeout are now set on every database connection** rather than inherited from a driver default.
- **Smart playlists are explicitly always current** — The editor no longer shows a “Live updating” checkbox. That setting was never consulted: playlists reevaluated against the current library whenever they were opened or exported regardless of its value, so an unchecked box falsely promised a frozen snapshot. Existing rule JSON and databases remain compatible, and subsequent rule saves normalize the legacy column without requiring a table rebuild.
- **Playlist import matching is deterministic, ambiguity-safe, and indexed** — An exact path decoded from a valid local `file:` URI—including Windows drive-letter form—wins first; non-file and malformed locations are not treated as filesystem paths. Otherwise title and artist, plus album when supplied, must match exactly after trimming and case normalization; matching is deliberately not fuzzy. A supplied duration is a hard inclusive ±5-second gate and only the unique nearest candidate wins, while an equal-nearest tie or duplicate metadata without duration remains unmatched. Import and later orphan reconciliation use the same resolver and build its path/normalized-metadata index once per library snapshot, so the result cannot change merely because two call paths ranked candidates differently and large playlists do not repeatedly scan and normalize the entire library. Path authority is retained only from imported location evidence; metadata-only imports and manually added entries remain fingerprint-only across repeated relinks, preventing a different song at a reused library path from silently taking over the entry. Corrupt negative or out-of-schema library durations are omitted from match evidence instead of wrapping or blocking reconciliation, and already-corrupt negative entry evidence is treated as absent.

### Fixed
- **Local and playlist playback now resolves exact identity into retained file authority at use** —
  PR #120 preserves the exact SQLite `tracks.id` string—including legacy non-UUID values—uses a
  frozen deterministic malformed-ID compatibility projection, and gives playlist rows the local
  `MediaKey` plus a separate `ViewOrigin`. Queue snapshots retain that identity, order, duplicate
  occurrence, and display metadata but deliberately omit the playback path. Initial Play, newly
  selected Next/Previous, repeat/replay, and output retries asynchronously re-read only the exact
  row, choose the most-specific root that is both currently configured and backed by complete,
  available, identity-confirmed state, and reject missing, empty, dead, timed-out, stale, or
  displaced authority without metadata, fingerprint, saved-path, or alternate-track fallback.
  Resolution retains the exact root, marker, ancestor chain, and regular-file handle under a
  five-second outer budget and rechecks the exact track path plus the root key, marker identity,
  confirmation, availability, and complete-scan authority state before publication. The
  observational `last_checked_at` scan timestamp is deliberately excluded from that authority
  snapshot, so a concurrent successful scan cannot reject playback merely by refreshing metadata;
  any authority-relevant drift still fails closed. The GTK
  handoff rechecks that the retained root is still the most-specific configured match without
  touching the filesystem, and the bounded ticket worker revalidates physical authority before
  every retained-handle clone. Local and AirPlay GStreamer, Chromecast, and MPD receive a typed
  lease and consume it through an opaque handle-backed app ticket, never by reopening the database
  path. The bounded two-chunk stream uses explicit-offset reads, so sequential or concurrent full
  and Range requests cannot corrupt one another through an OS-cloned shared cursor. Replacing a
  pathname cannot retarget an admitted load, while root or marker loss blocks later handle clones.
  Replacement, Stop, load or playback failure, terminal queue completion, ticket drop, and
  output/server teardown revoke future lookup; a failed item remains retryable through a fresh
  exact-ID resolution. Shared Chromecast cleanup revokes credential and retained-authority routes
  while preserving the documented server-lifetime contract of legacy explicit-file routes. At PR
  #121, GTK rows could still retain current paths for non-playback display and file management, and
  embedded-art display still gave its helper the exact playback-time path; the follow-up below
  closes that retained-authority boundary.
  Fifty-six focused exact-ID, root-authority, queue/view-ownership,
  handle-streaming, GStreamer-ticket, and MPD-ticket regressions pass with the locked all-target
  compile, formatting/diff gates, and strict warning-free Clippy in both profiles. The complete
  debug and release suites each pass 20 library, 804 application, and 10 repository-metadata tests
  (834 total), including every socket-bearing regression. Gemini's review follow-up is covered by
  a deterministic regression that accepts timestamp-only drift while independently rejecting
  changes to every retained root-authority field.
- **Local and playlist embedded art now stays inside retained file authority** — Artwork begins
  only after the exact local/playlist ID resolves beneath the still-current configured root, its
  resolution generation remains current, and the selected output accepts the load. The background
  worker receives a clone of that `ResolvedLocalMedia`, not a path or file URI. Cloning revalidates
  the root marker, ancestor chain, and exact regular file, and the worker owns the capability
  through parsing; replacement at the pathname therefore cannot retarget artwork and authority
  drift fails closed. Because cloned operating-system file handles may share a cursor, every parse
  attempt rewinds the retained file and leaves it rewound. Lofty uses the safe extension only as a
  format hint, content-probes an unknown suffix, and disables unrelated property reads. The
  explicit MP4 reread and raw `covr` fallback consume the same handle. The raw fallback reads at
  most 256 MiB, every parser path returns at most 32 MiB of artwork, and atom offsets, extended
  sizes, conversions, and additions are checked before slicing or allocation. Exact album-art
  generations also reject a result superseded during parsing. At that retained-art slice, the
  direct file-URI helper remained a transitional boundary for removable and OS-opened external
  files. All 9 focused album-art tests pass, including real tagged FLAC, path replacement,
  authority drift, cursor restoration, delayed generation, and malformed MP4 bounds. Locked all-target/all-feature
  check, strict debug/release Clippy, formatting, and diff checks pass; locked debug and release
  suites each pass 20 library, 872 application, and 10 repository-metadata tests (902 total). This
  closes one part of P3.1's compound final record without changing **219/223 (98.2%)** overall or
  **29/30 P3**. The Radio-Browser and external-file follow-ups close two more parts; the final
  removable adapter now closes the record and advances P3 to **30/30**.
- **Subsonic failed envelopes no longer retain a server-controlled error message** — HTTP-200 API failures keep only the numeric Subsonic code in a fixed typed error; code 40 remains authentication rejection and code 41 remains the HTTPS-only legacy-auth negotiation signal. A malicious or broken peer can no longer echo a submitted password or arbitrary text into retained errors, logs, or UI-facing classification.
- **Radio-Browser and geolocation no longer trust successful-looking JSON on an HTTP error** — Station and all three IP-geolocation provider paths now require a success status before their bounded response reader or deserializer can publish data. A `503` carrying a syntactically valid station or location is rejected, the geolocation cascade advances to its next provider, and late or oversized bodies remain bounded. Public same-origin/cross-origin redirects retain the existing no-`Referer`, no-HTTPS-downgrade policy.
- **Jellyfin and Plex now preserve reverse-proxy base paths for every API and media request** — Both clients remove only one trailing empty base segment before appending API paths, so root, `/share`, `/share/`, and already-escaped prefixes do not gain a doubled slash or lose their prefix. Plex stream-part and thumbnail paths now append below that same configured base instead of replacing it; escaped bytes are preserved and normalized dot segments cannot escape the prefix. Root `//` and prefixed `/share//` regressions pin the same one-empty-segment rule across catalogue and protected-media construction. The rejection uses a fixed peer-path-free error. Complete prefixed backend fixtures prove authenticated ping/identity, discovery, pagination, stream, and artwork construction.
- **Recycled sidebar rows and asynchronous UI results have production-boundary stale-authority coverage** —
  The existing one-handler sidebar invariant is strengthened with one parameterized `GAction`
  installed during factory setup; each bind replaces its immutable delete, disconnect, or menu
  target and unbind revokes that target before GTK recycles the widget. The display-independent
  regression activates that exact production
  action across repeated manual, unbound, connecting, DAAP, and playlist-header bindings, so it
  runs on every headless native CI host rather than depending on a display server. A synchronized
  loopback fixture also pauses a real request inside the persistent artwork worker, installs a
  newer generation, and proves the stale response is discarded before publication while only the
  newer bytes cross the completion channel. The production source cache and eviction boundary is
  exercised with reversed same-key results: an old loaded or missing callback cannot overwrite or
  remove the newer projection, and the newest inactive-source result remains cache-only.
- **Packaged-probe accepted sockets honor their bounded deadlines on Windows** — Winsock can retain a nonblocking listener's mode on an accepted socket, so a fragmented protected-media request could return `WouldBlock` immediately instead of waiting for its next bytes or the configured deadline. The probe now explicitly restores blocking I/O before installing read/write deadlines and treats any socket-configuration failure as fatal; timeout, malformed-request, and response failures remain distinct from the narrow teardown cancellation case.
- **Local Windows packaging could reject the Soup plugin it had just copied** — The bundle path
  remained relative until PE import inspection. PowerShell resolved the successful copy check
  against its current location, while `.NET` resolved `Path.GetFullPath` against the process
  working directory, which does not follow an interactive PowerShell `cd`. The inspector therefore
  received an absolute path to a different, nonexistent `libgstsoup.dll` and stopped with
  `PE import-inspection target must be an absolute existing DLL or EXE`. The bundler now resolves
  the created distribution through PowerShell's filesystem provider and retains its physical
  `ProviderPath` before constructing any PE target. Regression coverage reproduces a
  process/current-location split inside a repository path containing spaces, proves a custom
  FileSystem PSDrive does not leak its provider-only drive name to `.NET` or external tools, and
  retains the caller-relative `dist\tributary-windows` workflow.
- **Windows PE-import targets cross an explicit typed batch boundary with predicate-specific
  diagnostics** — PR #124 named every argument to `Invoke-BoundedPeImportBatch` and passed native
  x86_64 and ARM64 package/probe jobs, but the 2026-07-18 affected-host rerun rejected the same Soup
  singleton. That reproduction disproves positional array binding as the cause. The current repair
  candidate removes the non-terminal `[string[]]` function boundary: callers construct explicit
  `List[string]` instances for the singleton, every dependency-closure round, and each bounded
  process batch. Each target must be nonempty, quote/control-free, rooted,
  `GetFullPath`-valid, a DLL or EXE, and an existing file before process launch. Failure identifies
  the violated predicate in a bounded single-line report that retains at most 192 sanitized target
  characters plus a fixed truncation marker. The architecture-local `llvm-readobj` still inspects
  PE data without executing the target, and the established batch, argument, output, process,
  process-tree, and whole-closure
  limits remain intact. The current focused filters pass 14 `windows_*` and three `powershell_*`
  tests; the target-batch behavior regression uses `pwsh` away from Windows and is designed to
  require Desktop PowerShell 5.1 on native Windows. AST parsing, formatting/diff, locked check,
  strict debug/release Clippy, and complete 926-test debug/release profiles pass. Native package
  CI/Desktop PowerShell 5.1 execution and the exact affected-host rerun remain required, so this is
  not yet a claim that the reported packaging failure is fixed.
- **Malformed or expired DAAP catalogue responses now fail closed** — Tributary's DMAP parser no longer accepts a valid item prefix while silently discarding a malformed or truncated remainder in a known nested container, and every known integer field must use its exact protocol width. Wrong or missing containers, truncated framing, excessive nesting, short or overlong `mstt`, and duplicate response statuses now produce fixed-size typed parse failures before a catalogue can be published or any media request can begin. HTTP 401/403 share one authentication/session-expiration classification across the post-login update, databases, and items routes; in-band `mstt` 401/403 has the same typed result, any other explicit non-200 `mstt` is a typed connection failure, and a missing status on login or later session responses remains compatible with older peers. Once login yields a usable session ID, any later failure performs one bounded best-effort logout even when update or database discovery prevents construction of a client. The socket fixture reads complete request headers in deterministic fragments under a 16 KiB cap and five-second deadline and owns its handlers; lifecycle regressions cover nine malformed catalogue forms, eight expiration route/status combinations, and a non-authentication status, proving direct failed initial sync returns no backend, begins no stream/artwork request, and issues exactly one logout. Separate registry tests prove only explicit successful retention installs a source, replacement logs out the displaced session, and shutdown/release races remain joined and exactly-once.
- **Bundled runtime caches no longer modify the application install** — Windows and macOS now select GStreamer's writable registry before toolkit initialization under the operating system's per-user cache directory, separated by platform, architecture, and install path. Explicit unversioned or GStreamer 1.0-specific registry/plugin overrides—including an intentionally empty value—remain authoritative, and failure to establish the preferred Windows cache leaves GStreamer to its normal user-scoped default rather than falling back beside the executable in Program Files. Cache destinations are resolved through existing ancestors before creation and rejected if a normal or symlinked path would enter the application install. Portable regression fixtures derive native absolute paths on each host, so Windows CI exercises drive-qualified cache, install, and loader paths rather than Unix-shaped stand-ins.
  - The macOS launcher no longer exports a bundle-local GStreamer registry or copied `loaders.cache`. Instead, the signed app bundles and signs `gdk-pixbuf-query-loaders`; early startup invokes that exact helper with the exact absolute relocated loader modules and drains its output under a deadline and fixed memory bounds. Validation parses the cache's module/info/MIME/extension/signature record structure, including standalone quoted empty MIME or extension lists, instead of mistaking every standalone quoted line for a module. Exact absolute module records are retained; a safe helper-relative record is accepted only when resolving it against the signed helper's top level produces the exact expected loader set. Every accepted module is rewritten to its C-escaped absolute installed path, while malformed, incomplete, duplicate, traversal, or out-of-directory records fail closed before the user cache is atomically replaced. The old cache survives helper, validation, write, sync, or rename failure.
  - macOS packaging removes both mutable caches before signing and makes every signing and strict deep verification failure fatal. It launches a read-only signed copy from a path containing spaces with a fresh explicit probe cache, decodes the bundled app PNG through GDK-Pixbuf, initializes GStreamer and finds the bundled `playbin3` factory, proves both caches reference the relocated bundle and none appeared inside it, then strictly verifies the launched copy and untouched packaged app again before DMG creation.
  - The independently locked fuzz workspace is synchronized with the new target-specific dependency, so CI's strict `--locked` fuzz formatting and Clippy gates continue to validate the committed graph.
- **Linux file association now actually works** — The desktop entry advertised audio MIME types since 0.5.0, but its `Exec` line lacked a field code, so activating Tributary for a file never passed the URI to the process. `Exec=tributary %U` fixes it, and the required `AudioVideo` main category accompanies the existing `Audio`/`Music`/`Player` additional categories, so the entry passes `desktop-file-validate` (now enforced in CI).
- **Linux packages no longer install onto systems that cannot run the binary** — The `.deb` and `.rpm` declared GTK ≥ 4.14 and libadwaita ≥ 1.5 while the binary hard-requires GTK 4.16 and libadwaita 1.6, and the Arch manifest declared no version floors at all, so package managers could install a binary that could not start. Debian, both RPM paths, and Arch now state the real runtime minimums; the handwritten RPM build requirements carry the same floors.
- **Selecting an AirPlay receiver no longer falls back to a subprocess that cannot reach it** — When GStreamer's `raopsink` element was missing (the default on most Linux installations, because it ships in `gst-plugins-bad`), Tributary silently piped decoded audio into a spawned `shairport-sync` process. `shairport-sync` is an AirPlay *receiver*, not a sender: it ignored the device the user selected, so the click looked successful while nothing could ever play on that device — and the subprocess probe, spawn, and teardown all ran synchronously on the GTK main thread. The fallback is removed. A missing `raopsink` now fails the load immediately with a localized error naming the `gst-plugins-bad` package, translated in all 13 supported catalogs. Playback errors are also now surfaced as in-app toasts instead of being visible only in logs; every message an output emits was already reduced to a fixed, credential- and URL-free category, so displaying it verbatim is safe.
- **mDNS-discovered HTTP servers no longer depend on a second `.local` lookup for API and protected-media connections** — Tributary now retains a bounded, canonical snapshot of the socket addresses advertised for Subsonic, Plex, and DAAP services and applies it only when opening a direct connection. The advertised hostname remains in the URL, HTTP `Host`, TLS SNI/certificate identity, and exact-origin redirect checks; system and explicit proxies retain their normal behavior. Discovery preserves the authoritative SRV hostname, treats Avahi conflict-suffix cleanup as display-only, applies scheme-correct default ports, aggregates duplicate instances through a bounded origin index, publishes address updates, namespaces same-origin rows by backend protocol, and removes an origin only after its final retained exact service instance disappears within those bounds. Unauthenticated state is capped at 512 publications, 32 instances per origin, and 16 route addresses. Routes remain ephemeral and are snapshotted per connection into the applicable API/auth client and protected stream/artwork pools; connection ownership is minted before async work is queued, so a final service loss clears the route and revokes pending or active ownership even when a manually saved row remains available. Jellyfin's client accepts the same route contract, but its UDP discovery currently supplies only a URL. Automated tests preserve the hostname and HTTP `Host`; they do not perform a real TLS certificate/SNI handshake. This removes an identified resolver dependency but is not yet proof that DNS caused the reported live Windows failures.
- **Protected DAAP and Subsonic requests now survive reverse-proxy base paths** — DAAP stream and artwork construction previously erased a configured prefix such as `/share`, even though control and catalog requests preserved it; Subsonic preserved the prefix but produced `/share//rest/...` when the configured URL ended in `/`. Both now append beneath the exact existing path after removing only its optional trailing empty segment, preserving already-escaped bytes without double encoding. Protected DAAP stream and artwork requests also retain the same four fixed protocol headers as the control session instead of losing them at the typed media boundary. The app-owned proxy and artwork worker install those request-owned values and authentication separately, and a playback receiver cannot override them or forward anything except `Range`. Deterministic fixtures additionally prove Subsonic's HTTP-200 failed envelopes remain typed errors and an explicitly selected upstream proxy receives the exact protected request.
- **Protected DAAP and Subsonic playback could repeatedly fail after exactly 15 seconds** — Both backends converge on the same loopback proxy before local GStreamer playback; each protected load previously built a fresh upstream HTTP client, waited without distinct startup/body-idle deadlines, and could be abandoned by `souphttpsrc` at its 15-second default with only “Audio pipeline error.” Each GStreamer output now reuses credential-free default and immutable route-keyed upstream connection pools across track tickets. Connect establishment is bounded at 5 seconds, dispatch through response headers at 10 seconds, and silence between body chunks at 10 seconds, while an active stream has no total lifetime; the downstream loopback source uses a 30-second budget so the app-owned proxy returns a deterministic empty 502/504 first. Exact-origin redirects, Range-only forwarding, ticket revocation, and credential isolation are unchanged. Proxy failures now include only closed phase/category, numeric HTTP status when applicable, and bounded elapsed time; GStreamer reports only a closed domain, numeric code, protected flag, elapsed time, and normalized network/decoder/output/pipeline category, and stops its watch after the first terminal error.
- **Connection timeouts were mislabeled as authentication failures** — Sidebar, manual-dialog, passwordless DAAP, and environment-configured remote connection flows used authentication wording for every backend failure and retained raw backend/server text in logs and UI. They now classify authentication rejection, connection failure, timeout, invalid response, unsupported authentication method, and other backend failure from the typed `BackendError`, and expose only a fixed category and fixed user message. A transient connect timeout can no longer look like rejected credentials, and a server-supplied string, URL, or local I/O path cannot enter those diagnostics. The new remote and playback failure messages are translated in all 13 supported catalogs, with automated no-fallback and interpolation checks.
- **Failed preference writes no longer risk truncating `config.json`** — Configuration is serialized before touching the destination, written and synchronized through an exclusively created same-directory temporary file, then atomically installed. A serialization, write, sync, or rename failure preserves the previous file, and correctness-dependent library-folder actions update in-memory UI state only after the new configuration is safely persisted.
- **Playlist positions could be corrupted when upgrading from v0.5.0-era databases** — The position-normalizing migration used a self-referential rank update and ran outside a transaction, so a database with gaps, duplicates, or out-of-order rows could end up with a non-contiguous or reordered playlist. It now materializes a snapshot, normalizes and creates the unique index in one transaction, and preserves the intended order.
- **XSPF export could truncate a good destination before discovering an error** — Tributary now renders and validates the complete document first—including rejection of XML 1.0-forbidden control characters—then writes it to an exclusively created random sibling, flushes and `fsync`s it, and atomically replaces the destination. Serialization, write, and rename failures preserve the previous file and clean up the temporary file. A corrupt negative stored duration or one outside Tributary's supported `u64` millisecond range is omitted with a warning instead of blocking every otherwise valid track, because XSPF makes duration optional. XML generation and filesystem I/O run on a blocking worker, and both successful exports and errors are surfaced in the UI instead of failures being log-only.
- **Playlist import could silently create a partial or misleading playlist** — The old flow hid parser/database failures in an `Option`, treated matching query errors as no-match, created the playlist separately, ignored each entry-insert result, discarded unmatched source rows, and exposed whatever survived in the sidebar. Import now reads one transactional library snapshot and atomically commits the playlist plus every usable entry or rolls all of it back. Unmatched rows retain a path only when it came from a valid decodable local `file:` URI, plus normalized metadata/duration, in original order for later reconciliation; non-file or malformed locations may still match by usable metadata but are never preserved as paths. Rows with no usable identity or a valid duration too large for the database schema are rejected and counted, while a syntactically invalid or out-of-`u64` XSPF duration rejects the document before the transaction. The sidebar updates only after commit, and the completion alert accounts for every parsed source row as matched, preserved-unmatched, or failed while parse, worker, and database errors receive explicit rollback alerts.
- **A library scan could delete your metadata** — Initial reconciliation deleted any track it did not see, so an unreadable directory, an unmounted drive, or a permissions error would silently remove those tracks — along with their play counts and date-added — from the library. Reconciliation now tracks traversal completeness per root and refuses to delete on an incomplete view. Legacy roots with remembered metadata and confirmed roots whose identity can no longer be verified remain unavailable until the explicit main-window trust flow succeeds; any trust request whose complete scan finds no supported audio also pauses for stronger confirmation. File similarity is never accepted as proof. The engine checks the exact configured path, private prompt evidence, and expected persisted state; creates a random versioned root-owned marker or adopts an already-valid one; freshly probes identity and mount state; and only then atomically compare-and-swaps the expected database state. It discards pre-marker scan evidence, and the marker-backed conversion changes no track rows: it performs zero track upserts and zero stale deletions. A distinct complete ordinary scan may reconcile immediately afterward. Deliberately declined requests remain protected and suppressed for the current process; stale, failed, changed, or incomplete engine attempts remain fail-closed, preserve remembered metadata, and release the request for a refreshed retry.
- **Library-root replacement and remount races no longer authorize stale database changes** — Each marker-backed root capable of authorizing mutations retains one exact root-and-marker authority lease for its initial scan or watcher batch, then resolves mutation-bearing files, directories, and missing names beneath that lease. Audio metadata is parsed from the retained file rather than by reopening its path. Resolution rejects symbolic links and Windows reparse points, path escapes, cross-filesystem or cross-mount traversal, same-content marker replacement, same-marker root replacement, and parent, ancestor, or descendant substitution. Authority-promoting root-state changes, initial upserts and deletions, watcher upserts and deletions, and file and directory renames all revalidate their retained evidence after applying SQL changes and immediately before commit. Filesystem-touching authority work runs on blocking workers while the original retained handles stay live through commit or rollback, so those probes no longer occupy Tokio's async worker threads. A blocking-task failure rolls back or rejects the current work without falsely marking the root replaced; watcher failures also schedule reconciliation. Failed revalidation rolls the transaction back and publishes no success event.
- **Renaming a file or folder no longer loses its identity** — A rename previously deleted the old row and inserted a new one, discarding the track's ID, play count, date added, and playlist membership. Authoritative rename pairs (including whole-directory renames) now retarget the existing rows in one transaction, and an already-playing queue or open playlist re-resolves to the new path by stable track ID.
- **Deleting a track no longer corrupts the playlists containing it** — `playlist_entries` now holds a real foreign key with `ON DELETE SET NULL`, so an entry keeps its position and match fingerprint instead of retaining a dangling track ID.
- **Files changed during startup are no longer missed** — The filesystem watcher is now installed before the initial scan, and events that arrive during it are buffered and replayed. Watcher errors, overflow, and dropped backends fall back to an authoritative rescan instead of leaving the library silently stale.
- **DAAP sessions expired mid-use** — The backend and its session were dropped after library synchronization, so playback from a DAAP share failed with an expired session. The session is now retained for as long as the source is connected, and logged out exactly once on disconnect.
- **Playback followed the wrong track after sorting or filtering** — The playback queue was identified by visible row position, so re-sorting or filtering the view changed which track was "playing" and what Next/Previous meant. Playback now holds a queue snapshot keyed by stable source and track IDs.
- **Pressing Play on a newly loaded library could abort the application** — Idle Play selected its `StartAt` action through a `RefCell` read embedded directly in a `match` scrutinee. Rust retained that immutable borrow through the selected arm, where queue installation immediately requested a mutable borrow and panicked; on Windows the panic crossed the GTK callback boundary and aborted the process. Request selection now completes behind a function boundary before dispatch, guaranteeing that the read is gone before a fresh queue is installed. The Stop-then-Play regression now exercises the real `RefCell` boundary and verifies that immediate mutable replacement succeeds.
- **Chromecast commands could race each other** — Load, play, pause, seek, volume, and stop are now serialized through one ordered worker, so a stale load can no longer replace newer media, and a failure can no longer report Error and then Buffering.
- **A silent Chromecast can no longer pin playback control forever** — Cast control previously used `rust_cast`'s socket-hiding high-level client, so a receiver that accepted a connection but stopped replying could block the sole ordered worker indefinitely; Stop, a replacement Load, and even output shutdown remained queued behind it. Tributary now uses `rust_cast`'s public generic message manager and channel constructors over its own deadline-aware TLS transport, without a fork or new dependency. mDNS discovery carries a deterministic usable numeric IPv4 socket address into playback instead of doing a later unbounded `.local` lookup. TCP connect is bounded to 5 seconds; every high-level TLS/Cast operation has one absolute 8-second budget across every write and read plus a 2-second idle-I/O cap, so trickled bytes cannot renew the deadline. The `Rc` session remains entirely on its dedicated worker. Transport, TLS, framing, decoding, and deadline failures discard the desynchronized session immediately—including when superseded—while complete semantic rejections retain only enough synchronization for bounded best-effort cleanup, and all receiver-controlled text remains out of diagnostics. Real silent-peer regressions prove Stop, replacement Load, and Shutdown make bounded progress.
- **MPD playback could race commands and report guessed state** — MPD output now owns one ordered worker and persistent connection per load, tracks the exact `addid`/`playid` song identity, and publishes state, position, duration, and completion from authoritative receiver status; command and transport failures become opaque errors. Pause resumes the same song, an explicit remote Stop remains restartable, and natural queue exhaustion emits end-of-track. New loads no longer clear the shared queue, queue removal targets only Tributary's stable song ID, and controls revalidate the current ID before acting; when polling observes another client's replacement, Tributary relinquishes ownership without clearing that client's queue. Each owned load disables MPD's global `repeat`, `random`, `single`, and `consume` modes so Tributary can identify queue exhaustion itself. Name resolution, protocol input, resolved-address counts, media-URI sizes, idle I/O, post-resolution operations, and worker ingress are bounded; desynchronized connections are discarded instead of reused. Load-time global option side effects and the unavoidable shared-partition race between status revalidation and MPD's global pause or stop commands remain tracked follow-ups.
- **MPD cleanup no longer mistakes every ACK for a missing song** — A complete ACK previously retained no code, and targeted cleanup treated every synchronized rejection—including permission and bad-argument failures—as though the queue ID were already absent. Tributary now validates the single-command index and echoed command, parses the protocol's numeric ACK category into a closed redacted type, and recognizes only code 50 (`NoExist`) as absence evidence; permission, argument, password, system, valid-but-unknown, and all other correlated rejections remain real failures without retaining daemon-controlled text, while malformed or mismatched ACK-like input poisons the connection. Stopped/no-current after an owned item was observed active produces end-of-track only when targeted `deleteid` succeeds, proving the queued ID still existed and was atomically removed. MPD cannot distinguish natural exhaustion from an external Next past the final item, so both intentionally share that completion result. A `NoExist` response proves only that the ID was already absent and is reported as ownership loss without a false completion event. If a foreign-song status response becomes stale while a replacement Load is queued, the old session is now discarded before replacement cleanup can target the deliberately retained ID.
- **MPD command bursts no longer grow an unbounded worker queue** — Commands now enter a capacity-64 epoch-aware deque signaled by a capacity-one wake token, so the GTK-facing path uses only a short-held in-memory lock and never waits for MPD I/O or channel capacity. FIFO is exact below the cap. A newer Load, Stop, or Shutdown epoch atomically purges obsolete backlog and rejects late stale commands. Only a saturated same-epoch transient burst is reduced: consecutive seeks and playback transformations are folded without crossing non-transient command boundaries, then the oldest remaining transient is evicted if necessary so the newest intent survives. Stale wake tokens retain one absolute receive deadline instead of postponing status polling, and receiver loss clears pending work. Queue insertion now retains the same short-held mutex through nonblocking wake publication, so a worker cannot consume terminal Shutdown and drop the wake receiver between those two steps, making an accepted command spuriously report `Disconnected`. A real TCP regression withholds Pause's ACK while later Seek and Play commands enqueue, proves they are not pipelined, then verifies exact ordered execution after release; all terminal/ownership-loss variants also pass in both profiles.
- **MPD name resolution can no longer pin the ordered worker** — Blocking `ToSocketAddrs` lookup has been replaced by one process-lifetime resolver thread that owns a private GLib main context and every asynchronous GIO enumerator, callback, and callback-owned resource. Requests use a capacity-64 nonblocking channel and accept at most a 1-KiB host; empty, NUL-bearing, oversized, or overloaded work fails closed without retaining resolver text. Each context tick admits at most 16 requests, no more than eight GIO operations may remain active, and callbacks are dispatched between intake batches so queued work cannot starve completions. Numeric IPv4 and raw or bracketed IPv6 bypass DNS after deadline and lifecycle validation. Enumerated results preserve resolver order and IPv6 flowinfo/scope while deduplicating to a 32-address cap. The load or probe's one absolute budget now spans resolution, connection attempts, and greeting; deadline expiry, a newer Load/Stop/Shutdown epoch, or result-channel loss cancels GIO, and late callback sends and resource drops remain safely confined to the resolver thread. Nine net-new resolver regressions plus an expanded raw/bracketed IPv6 case cover validation, bounds, overload recovery, cancellation, callback progress and ownership, and address fidelity. Two additional real-socket regressions use channel synchronization without sleeps or fragile elapsed bounds: an accepted first peer that never greets proves a later address retains its fair share of the absolute deadline, and a real `::1` listener proves numeric IPv6 resolution, connection, greeting, and client/peer address families whenever the host supports the initial IPv6 bind.
- **Sidebar buttons acted on the wrong row** — Action handlers were reconnected on every list-item rebind, so one click could fire several actions, or act on a recycled row's previous source.
- **Smart playlists could not match a specific date** — A track's timestamp was compared against a rule's date as raw text, and a timestamp is never text-equal to a bare date, so a "Date Added **is** \<date\>" rule matched nothing at all. "Is after \<date\>" had the mirror-image bug and included the boundary day itself, which also skewed every "in the last N days" rule.
- **Smart-playlist limits could discard the requested final order** — The compound display sort ran before the limit's independent “selected by” order. The limit still chose the correct subset, but its internal selection sort ran last and replaced the requested presentation—for example, “top 25 most played, displayed by artist” stayed in play-count order. Rules now filter candidates first, the limit order selects and truncates the intended subset, and the compound sort orders only that retained subset. Item, duration, random-subset, multi-key, legacy-JSON, and end-to-end dynamic reevaluation tests cover the combined contract.
- **Properties now checks effective tag-write capability before enabling edits** — The dialog previously treated a supported filename extension as proof of writability, so Flatpak's read-only automatic Devices roots and ordinary permission failures surfaced only after Save. It now starts fail-closed, validates every exact selected path as a supported, readable, non-symlink regular file on a worker, independently proves every Windows file's installed DACL permits the later read/write/delete access, and rehearses exclusive creation, flush, replacement, and explicit cleanup with empty writer-owned siblings once per distinct containing directory, stopping at the first failed target or directory instead of performing needless later probes. A visible localized status enables the fields, MusicBrainz fill, and Save only after the complete selection passes; automatic-device failures explain the Flatpak custom-library route. Malformed or mixed local/remote selections can no longer silently edit only their valid subset, duplicate playlist rows are exact-deduplicated, and Save repeats the whole preflight before the first file write, so a member already read-only or unavailable at that check prevents any write. Save temporarily owns the dialog—Cancel and header close are disabled—and invalidates any in-flight MusicBrainz completion, including delayed label resets from an earlier lookup, until the write result restores a coherent state. Unix writer siblings start at mode `0600`, preventing a full source copy from being briefly exposed through a permissive umask before final permissions are applied. On Windows the source DACL is installed through an exclusive no-sharing handle before any audio byte is copied, preventing a permissive parent-directory ACL from creating the same exposure window. The real atomic writer still handles every operation as fallible because the check cannot eliminate unplug, permission, space, sharing, cleanup, or target-specific races.
- **Editing tags could silently discard changes, fail on valid audio, leave temporary files behind, or replace the wrong library identity** — Typing a non-number into Year, Track #, or Disc # rewrote the file, dropped that field, and reported success; invalid numeric edits are now rejected before the original is opened. Album-artist edits, which previously triggered a rewrite but were never applied, are now written too. Each save copies the original to an exclusively created randomized sibling, preserves its Unix mode on a best-effort basis or its snapshotted Windows DACL before copying content, `fsync`s the tagged copy, atomically replaces the original, and uses an RAII guard that attempts cleanup on every failure path, so concurrent saves cannot collide at a predictable temp name and failed renames take the same cleanup path. Cleanup I/O and process termination remain inherently fallible; any residual exact writer-owned name is excluded from scans and watcher admission. The initial randomized name ended in `.tmp`, which prevented Lofty from determining the copied audio format; the first extension-preserving correction then retained the complete source basename, which could exceed a filesystem component limit for an otherwise valid long name. The final bounded spelling is `.tributary-tag-<canonical UUID>.<case-normalized format extension>`. An audio-looking copy can also live beyond the watcher debounce while a large or remote file is being tagged; if indexed, its final rename could displace the original track row and lose stable identity, history, and playlist links. Full scans now reject only that exact writer-owned filename shape, standalone watcher events for it are ignored, and every supported temp-to-original rename form becomes a metadata upsert at the public path rather than an identity transfer. A committed generated-silence FLAC regression exercises the public write path and verifies all ten declared fields round-trip while the audio remains readable and no temporary sibling remains.
- **Browsing a USB device could index files that were not on it** — The old device scan followed
  symbolic links, so a stick containing `music -> /home/you` could list the home directory as part
  of the device. The registry-owned walker now retains exact mounted-root, ancestor, and final-file
  authority; it follows neither directory nor file links, rejects a descendant on another
  filesystem, and parses only through the exact opened file. At-use resolution repeats those
  boundary checks and rejects any relative ID absent from the accepted catalogue.
- **Removable media now follows the live native mount lifecycle without blocking discovery** — The
  one-shot platform-directory and drive-letter heuristics have been replaced by GIO's native
  `VolumeMonitor`. The monitor and every GIO object stay on GTK's main thread, where Tributary reads
  cached metadata only—discovery performs no path canonicalization, metadata probe, or filesystem
  traversal. Shadowed roots and roots without native-path access are excluded, as are mounts the
  backend explicitly classifies as network or loop; removable, ejectable, device-class, or
  otherwise user-unmountable native-path mounts are retained. Mount UUID, volume UUID, Unix device
  identifier, then root URI supply the best available logical key kept separately from the current
  native `PathBuf`, and aliases with the same key deterministically retain the lexically first path.
  Mount-added, changed, pre-unmount, and removed notifications reconcile keyed provenance claims for
  the window's lifetime. Pre-unmount and relocation disconnect the exact registry source before UI
  and playback invalidation; confirmed removal then releases the claim, while a failed unmount can
  reconnect from the fresh inventory only under a new epoch. An active source falls back to Local,
  and an immediate same-identity reappearance is reselected only while that exact automatic
  fallback remains current. Renames replace the row atomically at the same position, the Devices
  header tracks an empty/non-empty inventory, and window teardown disconnects every source before
  removing its global signal handlers.
- **Removable-device scans are lifecycle-owned and cooperatively cancellable** — Mount arrival asks
  `SourceRegistry` to construct the device adapter on Tokio's blocking runtime; selecting its row
  only displays the latest accepted catalogue or an empty projection while construction continues.
  The exact connect generation owns cancellation through deterministic traversal and bounded tag
  parsing; a stale, disconnected, replaced, or shutdown result cannot publish a catalogue. An
  accepted snapshot is pathless and exact-epoch-bound. Registry tests deliberately queue a scan
  behind a blocked worker, then prove disconnect cancels it without publishing a failure and
  shutdown joins it before rejecting later construction. Cancellation cannot interrupt the single
  filesystem or parser call currently executing, but it is rechecked throughout traversal and at
  lifecycle publication; GTK performs no scan polling or per-track filesystem work.
- **Slow source work could replace newer navigation or stale playlists** — Playlist and
  smart-playlist loads, radio searches and consent, local-library debounces, and pending remote
  connections carry exact source/view plus navigation generations. Removable construction and
  scanning now use the stronger registry contract: the exact connect generation, source epoch,
  media lease, and accepted catalogue must all remain current before publication or resolution. A
  completion may update a cache only when it is newest for that owner and may render only for the
  exact current navigation, so switching away or reselecting cannot let older work render last.
  The newest accepted result for an inactive source may still remain cached for a fast return.
  Pending remote authentication remains the current navigation intent while the prior source is
  visible; that visible source keeps its own exact projection generation so browser and status
  updates continue during authentication, while older away-and-back callbacks remain rejected.
  Background publication and playlist/sidebar refreshes cannot unexpectedly select another source,
  and a rejected second connection restores the pending server's selection. After a committed
  library mutation and playlist reconciliation, Tributary invalidates outstanding playlist
  generations and cached projections, then reloads the active playlist only if it still owns
  navigation, preventing reminted, relinked, or orphaned track IDs from leaving stale actionable
  rows. Transient playlist database failures preserve the last valid cache and display instead of
  replacing them with an empty result.
- **Plex tracks without media parts no longer appear playable** — The playback-time resolver initially issued every cached Plex track an opaque playback reference even when Plex supplied no `Media`/`Part.key`, turning a previously disabled row into an asynchronous resolution failure. Tributary now omits tracks with no non-empty media part from the playable catalogue, searches every returned media/part entry for the first usable locator instead of assuming the first entry is valid, and takes bitrate/format metadata from that same selected media entry.

### Known limitations
- **Chromecast publication is currently IPv4-only** — Tributary's receiver-facing media-ticket listener is IPv4-only. An IPv6-only control endpoint would connect but could not fetch the advertised media and is therefore omitted.
- **A shared MPD queue may retain Tributary's old entry after another client takes over** — Once status reports a foreign current song, Tributary deliberately relinquishes ownership without issuing `deleteid` for its former stable ID. MPD offers no conditional “delete this ID only if it is still not current” operation, so another client could select that entry between status revalidation and deletion; removing it then would disrupt playback Tributary no longer owns. A direct-media entry may remain playable; a protected entry may remain selectable but its revoked opaque ticket will fail. No retained entry contains the backend credential, but it can remain until manually removed. Automatic orphan cleanup requires the still-open detectable exclusive-control mode, which must also address MPD's global pause/stop and load-option side effects.
- **Native removable-media identity and cancellation have deliberate limits** — GIO does not expose
  a uniformly reliable physical-USB flag. Tributary's broad `can_unmount` fallback may include a
  non-removable or natively mounted network filesystem when backend class metadata is absent, while
  a platform backend can omit a device it cannot classify. UUIDs identify a logical filesystem
  rather than unique hardware, so a clone may collide; Unix-device and root-URI fallbacks can change
  with device or path assignment. The app does not automount unmounted volumes, eject devices, or
  browse pathless/MTP-only media. Adapter cancellation cannot interrupt the one filesystem or
  tag-parser call already executing, although it is checked during traversal and before lifecycle
  publication; nested filesystems are now rejected rather than traversed. Cross-platform
  add/change/pre-unmount/failed-unmount/reconnect/remove behavior has deterministic policy,
  authority, registry, and UI coverage but has not yet been manually validated with physical
  hardware. In Flatpak, inventory can still list an eligible native mount outside `/media`,
  `/run/media`, and `/mnt`, but the sandbox grants automatic Devices file access only beneath those
  roots and only read-only; an inaccessible listed root fails construction. Because removable rows
  no longer retain a host path, Properties is deliberately unavailable until a typed mutation
  authority exists. Pathless/MTP-only media remains unavailable, and interactive
  portal/custom-library behavior still needs a local installed-sandbox smoke test. Legacy direct
  roots must use **Reauthorize**, not ordinary remove-and-add, to preserve identity; the guarded flow
  intentionally rejects a confirmed legacy root without a supported durable marker or a markerless
  destination on which it cannot create one.
- **A markerless read-only library root cannot be enrolled** — Trust may adopt an existing valid root marker on read-only storage, but it cannot safely create a marker there. Such a root remains unavailable and its remembered metadata remains protected until the marker can be created on the intended storage.
- **A library-root marker identifies a logical library, not a unique physical device** — A clone that already carries the same valid marker before authority-lease acquisition is treated as the same logical bearer, preserving backup and restore semantics; marker identity is not proof of physical-device uniqueness. Once an eligible root's initial scan or watcher batch acquires its lease, retained root, marker, and descendant evidence reject later substitution. Filesystem changes and a SQLite commit cannot form one atomic transaction, so authorization linearizes at the final retained-handle validation inside the database transaction, after its SQL changes and immediately before commit. Positive object handles remain live through commit; a missing name is instead a point-in-time absence proof bracketed by validation of its retained parent. A filesystem change after that boundary is a subsequent transition for the watcher or reconciliation to process. On Windows, retained authority handles intentionally omit delete sharing: attempts to rename or delete the retained root, marker, or a briefly bound descendant can receive a sharing violation until the scan, watcher batch, or transaction releases the relevant handle. A final probe of slow or hung network storage can keep the SQLite writer transaction open while the blocking worker finishes, although it does not block Tokio's async worker threads. These controls provide consistency against ordinary edits, replacement, remount, and hotplug races; they are not a sandbox against a malicious same-user process with equivalent filesystem or mount privileges.
- **Source lifecycle is unified where a retained session or locator authority exists** — Stable typed source/media
  identity spans authenticated remotes, local/playlist views, Radio-Browser, removable media, and
  external files, and complete local and remote track-catalogue publication shares one
  `MediaBackend` trait-object seam. Subsonic, Jellyfin, Plex, DAAP, Radio-Browser, external files,
  and removable mounts now share one
  production `SourceRegistry` for connection/catalogue/view generations, session epochs, cancellation,
  sanitized failures, provenance, media revocation, disconnect, and shutdown. GTK renders its
  atomic baseline rather than owning a sibling registry or rebuilding state from row flags.
  Saved and Discovery claims remain independent, so removing Saved demotes a still-discovered row;
  discovery loss still revokes route-bound work while another claim may keep the logical row
  visible. Local/playlist playback resolves exact stable IDs into retained root/file authority at
  use, and local/playlist embedded art now clones that same authority only after output acceptance
  and retains its exact handle through parsing. Radio queues are pathless and resolve the exact
  current accepted-view locator at use. External-file queues are likewise pathless and resolve a
  registry-owned retained open-file capability at use; post-accept embedded art clones that same
  authority, and replacement/terminal transitions retire it explicitly. Removable queues are also
  pathless: their registry adapter resolves accepted lossless relative IDs under exact mounted-root,
  file, epoch, and lease authority for playback and embedded art. Hotplug transitions disconnect
  before UI/playback invalidation. Always-present Local remains a deliberately specialized engine,
  and playlist/radio browsing remains view state rather than a separate connection; these are
  intentional source shapes rather than a second locator/session owner. Typed tag-mutation
  authority for pathless removable rows remains a future capability, so their Properties action is
  omitted instead of crossing the lifecycle boundary with a path.
- **Credential-bearing media tickets remain replayable within their hard lifetime** — Each opaque output ticket is a bearer for one fixed media item and arbitrary byte ranges until the earlier of source-lease/lifecycle revocation or its absolute 24-hour expiry; it is not a one-shot token. Neither event cancels a response the proxy already admitted. Protected local/AirPlay tickets are reachable only through a dedicated loopback listener; a protected MPD load from an unspecified address, or a scoped/link-local IPv6 route, fails closed rather than exposing the upstream request. Playback-time local-authority tickets follow their owning load lifecycle; only legacy explicit-file routes retain the older server-lifetime capability contract.
- **Live packaged-Windows protected-playback validation remains outstanding** — Fake DAAP and Subsonic streams traverse the complete production protected-player path through real GStreamer, and native x86_64 and ARM64 package jobs both prove their finished distribution supplies the selected HTTP source, scanner, decoder, required DLL closure, direct-loopback policy, alternate-source fail-closed path, and real FLAC decode to end-of-stream without borrowing the build host's runtime. That deterministic probe cannot establish compatibility with the reported live DAAP/Subsonic servers, `.local`/mDNS routing, TLS, physical audio output, firewall or endpoint-security policy, or the user's legitimate upstream proxy. Audible live playback from the packaged application remains to be recorded, and no automated result proves that DNS caused the original failures.
  The exact affected Windows host previously reached the copied Soup plugin and then rejected its
  singleton PE-inspection target as non-absolute or nonexistent. PR #124's explicit argument
  binding passed both native CI package jobs but did not change that host result, disproving the
  positional-binding hypothesis. PR #127 now uses explicit `List[string]` values at
  every singleton/round/batch boundary and emits predicate-specific, bounded single-line target
  diagnostics while preserving nonexecuting inspection and every resource limit. Native x86_64
  and ARM64 package CI, including Desktop PowerShell 5.1 execution, passed in run `29648906031`.
  The exact affected-host rerun remains pending and must precede the audible playback check.

---

## [0.5.0] — 2026-05-08

### Added
- **Multiple music directories** — Tributary now supports scanning and watching multiple library folders simultaneously. The preferences UI allows adding and removing directories. Existing single-path configs are automatically migrated to the new `library_paths` format on first launch. (#31)
- **XSPF playlist import/export** — Right-click any playlist to export it as an XSPF file, or use "Import Playlist…" on the Playlists header to import external playlists. Imported tracks are matched against your local library using fingerprint-based reconciliation (title, artist, album, duration) for resilience against file path changes.
- **Default smart playlists** — Three smart playlists are automatically seeded on first launch: "Recently Added" (last 30 days), "Recently Played" (last 14 days), and "Top 25 Most Played".
- **Chromecast local file streaming** — Chromecast devices can now play local audio files. An embedded, LAN-only HTTP server (`axum`) serves files via UUID-keyed URLs — no path traversal possible. The server binds to the machine's LAN IP on an OS-assigned port and starts on-demand when the first local file is cast. Supports HTTP byte-range requests for seeking. Persistent Cast V2 connections with heartbeat keep-alive and media status polling for position tracking. (#1)
- **Window position persistence** — Tributary now remembers its window size and maximized state across restarts.
- **Windows 11 Snap Layout support** — The maximize button area now correctly triggers the native Windows 11 Snap Layout flyout via `WM_NCHITTEST` subclassing, enabling direct window tiling without losing GTK4's CSD appearance.
- **macOS "Open With" support** — Tributary now appears in Finder's "Open With" menu for audio files (MP3, FLAC, OGG, Opus, AAC, WAV, AIFF) via `CFBundleDocumentTypes` in Info.plist and `HANDLES_OPEN` application flag. (#36)
- **Linux file association** — Added `MimeType` entry to the `.desktop` file for audio MIME types.

### Fixed
- **Column display recycling bug** — Added `connect_unbind` handlers to all tracklist column factories to clear label text when rows are recycled, preventing stale data from appearing in the wrong row during rapid scrolling.
- **Playlist timestamps were epoch seconds, not RFC3339** — `PlaylistManager::now_rfc3339()` was writing bare `as_secs()` integers (e.g., `"1735689600"`) into the `playlists.created_at` and `updated_at` columns instead of ISO-8601 strings. Replaced with `chrono::Utc::now().to_rfc3339()`. Track timestamps in `engine.rs` were already correct.
- **Chromecast `toggle_play_pause` always sent Pause** — The handler unconditionally sent `CastCommand::Pause` regardless of current state, so OS media keys / spacebar could never resume Cast playback once paused. `current_state` is now wrapped in `Arc<Mutex<PlayerState>>`, optimistically updated on commands, and authoritatively refreshed from the device's `player_state` field on every status poll. `state()` now reflects the live device state.
- **Chromecast `state()` always returned `Stopped`** — `current_state` was initialized to `Stopped` and never reassigned. Wired the polling loop to update the shared mutex on every device status response.
- **Jellyfin search results showed broken cover-art for items without primary image** — `JellyfinBackend::search()` unconditionally generated `cover_art_url` for `MusicAlbum` and `MusicArtist` results even when `image_tags["Primary"]` was absent. `refresh_library()` already validated this correctly; `search()` now mirrors the same check.

### Removed
- **Smart-playlist `Most/Least Recently Played` limit sort** — These `LimitSort` variants existed in the enum and editor dropdown but had no `last_played` column to sort against, so they silently no-op'd at evaluation time. Removed the variants and corresponding UI options. They will be reintroduced once playback statistics (`play_count` increments, `last_played` timestamps) are persisted end-to-end.

### Known limitations
- **Auth tokens travel in stream/cover-art URLs for Jellyfin and Plex** — Tributary's debug logger redacts `api_key=` and `X-Plex-Token=` via `redact_url_secrets()`, but the URLs handed to GStreamer's `playbin3` (and to the album-art HTTP fetcher) are the unredacted originals. This is the auth model these servers expose for media URLs; the tokens may therefore appear in GStreamer's own debug output and in HTTP access logs upstream of Tributary. Mitigation: keep `GST_DEBUG` unset at runtime and prefer HTTPS endpoints. Subsonic uses a salted-MD5 token (not a bearer secret) so this consideration does not apply.

### Privacy
- **"Stations Near Me" requires opt-in IP geolocation** — As before, this feature sends one HTTPS request to a small cascade of public geolocation services (`ipapi.co`, `ipwho.is`, `freeipapi.com`) to derive an approximate location. The consent dialog (introduced in 0.4.0) gates the first request and the choice is remembered in `config.json`; declining keeps every other Tributary feature working offline-or-LAN-only.

---

## [0.4.1] — 2026-04-23

### Added
- **Nextcloud Music (Subsonic) compatibility** — Tributary now auto-detects servers that require legacy plaintext authentication (Subsonic error code 41) and automatically falls back to hex-encoded password auth (`p=enc:<hex>`). Token-based auth (`t=`/`s=`) is always tried first; plaintext fallback is only used when the server explicitly rejects token auth, and is **refused over plain HTTP** — only HTTPS connections are permitted. This enables Nextcloud Music, older Subsonic servers, and other legacy-auth-only implementations. (#25)

### Changed
- **Modularized `window.rs` (3,730 → 1,776 lines)** — Refactored the monolithic main window module into 6 focused sub-modules for improved maintainability and AI-assisted development:
  - `window_state.rs` — Shared `WindowState` struct bundling 16+ `Rc<RefCell<…>>` UI state fields for dependency injection across modules.
  - `playlist_actions.rs` — Playlist CRUD logic (create, rename, delete, reorder entries).
  - `output_switch.rs` — Output selector popover click handler (local, MPD, AirPlay, Chromecast switching).
  - `context_menu.rs` — Tracklist right-click menu (add/remove from playlist, Properties dialog).
  - `discovery_handler.rs` — mDNS/DNS-SD event handler (sidebar + output list add/remove for discovered servers, AirPlay receivers, and Chromecast devices). Deduplicated device-check logic into shared helpers.
  - `source_connect.rs` — Sidebar selection-changed handler (source switching for local, playlist, USB, radio, connected remote, and unauthenticated remote sources with auth dialog flows).
- **Dead code cleanup** — Removed orphaned `disable_popover_scrollbars` from `window.rs` (now only in `context_menu.rs`). Cleaned up unused imports (`sea_orm`, `AirPlayOutput`, `ChromecastOutput`, `MpdOutput`, radio functions, `show_auth_dialog`) that migrated to sub-modules.

### Fixed
- **macOS `.app` bundle: `xattr -cr` fails on ~60 missing files** — The build script copied GLib schemas and GDK-Pixbuf loaders with `cp -R`, which preserves Homebrew's Cellar symlinks. On the user's machine those symlink targets don't exist, causing `xattr: No such file` errors for every `.gschema.xml`, `.so`, and `.dylib` in the bundle. Changed to `cp -RL` (dereference) so real file contents are bundled. The icon copy already used `-RL`; this makes schemas and pixbuf loaders consistent.
- **macOS `.app` bundle: songs fail to play (GStreamer "not-negotiated" error)** — Three compounding issues: (1) The bash launcher wrapper set `GST_REGISTRY_UPDATE=no`, preventing GStreamer from scanning bundled plugins on first launch. Replaced with an explicit `GST_REGISTRY` path. (2) The `GST_REGISTRY` path used `$BUNDLE_ROOT/Contents/MacOS/` but `BUNDLE_ROOT` is already `Contents/`, resulting in a non-existent double-nested path (`Contents/Contents/MacOS/gst-registry.bin`). Fixed to use `$DIR/gst-registry.bin`. (3) A stale `gst-registry.bin` from the CI builder (containing `/opt/homebrew/` paths) was shipped inside the bundle because the cleanup step ran before code signing, and the signing process or a subsequent GStreamer invocation regenerated it. Moved the registry cleanup to after code signing so it's the last step before DMG packaging. Added a runtime defense in `setup_macos_bundle_env()`: before `gst::init()`, the binary now checks whether the registry file references the current bundle's plugin path and deletes it if stale, forcing a clean rescan.
- **macOS pixbuf loaders with `.dylib` extension not code-signed** — The rpath-fixing and ad-hoc code-signing loops only matched `*.so` files. After symlink dereferencing, loaders may be `.dylib` files. Both loops now handle both extensions.
- **Remote sources don't show tracks on first connection** — Connecting to a DAAP, Subsonic, Jellyfin, or Plex server would complete successfully (tracks fetched, sidebar icon updated) but the tracklist remained empty until the user switched away and back. The connection guard (`pending_connection`) was only cleared *after* `sidebar_selection.set_selected()`, but `set_selected()` triggers the `selection_changed` handler synchronously — which immediately hit the guard and returned early, never setting `active_source_key` or calling `display_tracks`. Moved the guard clear to before `set_selected()` so the handler can run.
- **Windows About dialog icon missing** — The Windows build script copied the app's hicolor icons into the dist folder but didn't rebuild `icon-theme.cache`. GTK used the stale cache from MSYS2 (which only indexes system icons) and couldn't find `io.github.tributary.Tributary`. Added a `gtk4-update-icon-cache -f -t` step after bundling the app icons.
- **macOS: audio fails on multi-channel output devices (e.g. monitors with spatial audio)** — On macOS devices where Core Audio reports more than 2 channels (such as the Dell S3225QC with 8-channel spatial audio speakers), GStreamer's `audioconvert` element would fixate to the device's maximum channel count (8) with `channel-mask=0x0` (no channel positions), then fail to build a 2→8 channel converter — causing every file to error with `"not-negotiated"`. Workaround: at player init, a pad probe is installed on `osxaudiosink`'s sink pad via playbin's `element-setup` signal. The probe intercepts `CAPS` queries and rewrites the `channels` field to `[1, 2]`, forcing `audioconvert` to preserve the source channel count instead of upmixing. This is a GStreamer bug (likely in `audioconvert`'s `fixate_caps` logic); the workaround is marked for removal once upstream ships a fix.

### Performance
- **Jellyfin library fetch ~10× faster** — Increased the API page size from 500 to 5,000 items (reducing HTTP round-trips from ~26 to ~3 for a 12,500-track library) and switched to concurrent fetching of tracks, albums, and artists via `tokio::try_join!` instead of sequential awaits. A 12,500-track library on a local network now loads in ~5 seconds instead of ~52 seconds.

### Security
- **Plaintext password redacted from logs** — `redact_url_secrets()` now masks the Subsonic `p=` query parameter (hex-encoded password used in legacy auth) before it reaches log output. Detection uses a multi-param heuristic (`p` + `u` + `c` present) to avoid false positives on unrelated URLs. Added 2 new unit tests.
- **`AuthMode` Debug hardened** — Replaced the `#[derive(Debug)]` on the internal `AuthMode` enum with a manual `Debug` impl that redacts all secret fields (token, salt, hex password), preventing accidental credential leakage if the struct is ever debug-formatted.

## [0.4.0] — 2026-04-19

### Added
- **Chromecast audio output** — New `src/audio/chromecast_output.rs` implementing the `AudioOutput` trait for Chromecast (Cast V2) devices using the MIT-licensed `rust_cast` crate. `OutputType::Chromecast` variant added to the output type enum. Discovery integration browses `_googlecast._tcp.local.` via mDNS and automatically adds discovered Chromecast devices to the output selector popover with `"video-display-symbolic"` icon. The friendly name is extracted from the mDNS `fn` TXT record field. Clicking a Chromecast row connects via TLS (port 8009), launches the Default Media Receiver app, and sends `media.load()` with the track URL. Supports play, pause, stop, seek, and volume control (0.0–1.0 linear). Remote sources (Subsonic, Jellyfin, Plex, radio) work out of the box since their stream URLs are already HTTP; local `file:///` URIs are rejected with a clear error message (embedded HTTP server for local file casting deferred to a follow-up). Includes 7 unit tests.
- **`-Test` flag for `build-windows.ps1`** — New quick-exit mode that sets up the MSYS2 build environment and runs `cargo test --release --target $RustTarget`. Useful for running the test suite from PowerShell without a full DLL-bundled build.
- **`-Run` flag for `build-windows.ps1`** — New quick-exit mode that builds in release mode and launches the compiled binary directly from PowerShell. Useful for quick iteration without manual DLL bundling.
- **Audio output abstraction** — New `AudioOutput` trait (`src/audio/output.rs`) that abstracts playback destinations (local speakers, MPD, future AirPlay). `LocalOutput` wraps the existing GStreamer `playbin3` pipeline. `MpdOutput` sends MPD protocol commands over TCP to remote or local MPD servers with newline-sanitised command injection prevention and 5-second connection timeouts. Designed for future AirPlay 2 (shairport-sync) extension.
- **MPD output backend** — `src/audio/mpd_output.rs` implements the `AudioOutput` trait for Music Player Daemon servers. Commands are sent on background threads to avoid blocking the GTK main thread. Includes `probe()` for TCP connectivity validation and 5 unit tests for the newline sanitisation security layer.
- **Output selector UI (iTunes AirPlay-style)** — New `MenuButton` with a `Popover` in the header bar (between volume slider and hamburger menu) showing available audio output destinations. "My Computer" is always present; MPD outputs are added via a "+" button and persisted to `outputs.json`. The output selector follows the iTunes/Apple Music AirPlay popover pattern with icon + name + checkmark rows.
- **Output switching wired** — Selecting an output in the popover now routes all playback commands (play, pause, stop, seek, volume) through the `AudioOutput` trait abstraction. `PlaybackContext` uses `Rc<RefCell<Box<dyn AudioOutput>>>` instead of a direct `Player` reference. The volume slider disables when `supports_volume()` returns false (e.g., MPD). The `Player::event_sender()` method allows non-GStreamer outputs to feed events into the same `player_rx` event loop.
- **Add Output dialog** — Clicking the "+" button in the output selector popover opens an "Add Output" dialog for MPD servers with Name, Host, and Port fields. The dialog probes the MPD server on a background thread (5-second TCP timeout) before saving, validating the `OK MPD x.y.z` greeting line.
- **Output selector row-click switching** — Clicking a row in the output selector popover now swaps the active audio output. Selecting "My Computer" (index 0) restores the local GStreamer output; selecting an MPD output constructs a new `MpdOutput` and routes all playback through it. The local output is parked (not destroyed) when switching to MPD and restored when switching back. Checkmark visibility updates to show the active output, the volume slider disables for MPD (which manages its own volume), and the popover auto-closes after selection.
- **Haversine geo-distance module** — New `src/radio/geo.rs` with a Haversine great-circle distance function, US state centroid lookup table (50 states + DC), and country centroid lookup table (~60 countries). All data is compiled-in with no external API dependency. Enables client-side distance estimation for radio stations that have country/state metadata but lack geographic coordinates. Includes 11 unit tests covering same-point, NYC→London, antipodal, symmetry, case-insensitive lookups, and interstate distance calculations.
- **Geo-distance sorting for Stations Near Me** — The "Stations Near Me" results are now sorted by estimated distance from the user using the Haversine formula. Stations with actual `geo_lat`/`geo_long` coordinates get exact distances; stations with only a US state name use state centroid lookups; country-only stations use country centroid lookups. This produces a much more useful ordering where nearby stations appear first regardless of which tier they came from.
- **AirPlay output scaffolding** — New `src/audio/airplay_output.rs` implementing the `AudioOutput` trait for AirPlay (RAOP) receivers. `OutputType::AirPlay` variant added to the output type enum. Discovery integration browses `_raop._tcp.local.` via mDNS and automatically adds discovered AirPlay receivers to the output selector popover (no manual "+" button needed). Streaming implementation is scaffolding-only — the GStreamer `raopsink` and `shairport-sync` pipe backends are stubbed with fallback error reporting. Includes 6 unit tests.
- **USB device scaffolding** — New `src/device/mod.rs` with `Device` trait, `DeviceInfo` struct, and `DeviceType` enum for portable music device abstraction. `src/device/usb.rs` implements platform-specific USB mass storage detection: Linux scans `/media/$USER/` and `/run/media/$USER/`, macOS scans `/Volumes/` (excluding system volumes), Windows enumerates drive letters via `GetDriveTypeW`. Includes 6 unit tests. (#1, #8)
- **AirPlay output wired** — Clicking a discovered AirPlay receiver in the output selector popover now constructs an `AirPlayOutput` and routes all playback through it. The GStreamer `raopsink` pipeline (`uridecodebin ! audioconvert ! avenc_alac ! raopsink`) is attempted first; if `raopsink` is unavailable, falls back to `shairport-sync` in pipe mode (Unix only). Host:port is stored on the ListBoxRow widget name during mDNS discovery for precise retrieval on click. Volume slider disables when AirPlay is active. (#7)
- **USB device sidebar integration** — Detected USB mass storage devices now appear under a "Devices" category header in the sidebar on startup. Clicking a USB device scans the volume for audio files using the `tag_parser` and `walkdir` on a background thread, then populates the tracklist with full metadata (title, artist, album, genre, year, bitrate, etc.). The browser panes filter USB device tracks the same way as local library tracks. Scanned results are cached so re-selecting a device is instant. (#1)
- **Album artist sort** — New `album_artist_name` field throughout the data pipeline: DB migration adds the column to the `tracks` table, `lofty` tag parser extracts the `AlbumArtist` tag, all backends (local, Subsonic, Jellyfin, Plex, DAAP) propagate it through to the UI. The browser `TrackSnapshot` includes a `browser_artist()` helper that returns album artist when enabled, falling back to track artist. A `group_by_album_artist` boolean preference is added to `config.json` (default: off). The smart playlist rule engine supports `AlbumArtist` as a filterable text field. (#5)
- **Smart playlist compound sort** — New `sort_order` field on `SmartRules` enables multi-key compound sorting of playlist results. Each `SortCriterion` specifies a field (Artist, AlbumArtist, Album, Title, Year, TrackNumber, DiscNumber, Genre, Duration, Bitrate, PlayCount, DateAdded, DateModified) and direction (Ascending/Descending). Criteria are applied in sequence — the first is the primary sort, subsequent criteria break ties. This enables Tauon-style generator code ordering like "Artist alphabetised → albums in chronological order → track number". When a limit is also configured, its independent selection order chooses and truncates the subset before compound presentation sorting. Fully backward-compatible: `sort_order` defaults to empty via `#[serde(default)]` so existing playlists are unaffected. (#21)
- **Keyboard shortcut: Ctrl+F / Cmd+F to focus search** — Pressing `Ctrl+F` (or `Cmd+F` on macOS) now focuses the browser search entry and makes the browser visible if hidden. (#3)
- **Connection guard for all network sources** — Clicking a sidebar server that is already connecting (spinner visible) no longer opens duplicate auth dialogs or spawns duplicate connection tasks. The guard applies to all backends (Subsonic, Jellyfin, Plex, DAAP) and prevents race conditions when impatient users double-click slow servers. (#13)
- **i18n/l10n framework with 13 languages** — All user-facing UI strings are now wrapped in `rust-i18n`'s `t!()` macro with compile-time YAML translation files in the `locales/` directory. The system locale is auto-detected via `sys-locale` at startup. English is the fallback language. Complete translations included for: Dutch (nl), French (fr), Spanish (es), German (de), Italian (it), Brazilian Portuguese (pt-BR), Polish (pl), Russian (ru), Japanese (ja), Korean (ko), Chinese Simplified (zh-CN), and Chinese Traditional (zh-TW) — covering GNOME Tier 1 language coverage. Column titles and config keys remain in English for persistence compatibility. (#20)

### Fixed
- **Windows i18n not loading (raw translation keys displayed in UI)** — `sys_locale::get_locale()` returns `en-US` on Windows, which doesn't match the `en.yml` locale file. Added locale normalisation that tries the full locale first (preserving `zh-CN`, `zh-TW`, `pt-BR` region-specific files) then falls back to the base language code (`en-US` → `en`). Underscore separators are unified to hyphens for cross-platform consistency (`zh_CN` → `zh-CN`). This also fixes sidebar source ordering, which was broken because untranslated header keys sorted alphabetically instead of in the intended display order.
- **`test_indiana_to_new_york_state` failing in CI** — Widened the assertion range from 800–1000 km to 800–1100 km to match the actual geographic centroid distance of ~1043 km (the New York centroid is in the Adirondacks, not NYC).
- **Non-English locale music folder not scanned** — The default library path now uses `dirs::audio_dir()` (XDG `$XDG_MUSIC_DIR`) which respects the system locale (e.g. `~/Musique` on French systems, `~/Musik` on German). Falls back to `~/Music` only if `audio_dir()` returns `None`. The library engine also reads the configured path from Preferences instead of hardcoding `~/Music`. (#22)
- **Genre displays as blank for tracks without genre metadata** — Tracks with no genre tag now display "Unknown" instead of an empty cell in the tracklist, matching Rhythmbox behaviour. (#10)
- **Last column not resizable** — The rightmost visible column in the tracklist auto-expanded to fill remaining space (a GTK4 `ColumnView` quirk), preventing the user from resizing it by dragging its right edge. Added an invisible zero-width sentinel column that absorbs the auto-expansion, so every real column now has a draggable resize handle. (#12)
- **macOS M4A album art not displaying on some Apple-encoded files** — Replaced the brute-force `covr` atom scanner with a proper recursive MP4 atom walker that follows the standard `moov → udta → meta → ilst → covr → data` path. Now correctly handles the `meta` full-box version/flags prefix, extended 64-bit atom sizes, and deeply-nested atom layouts. The brute-force scan is retained as a fallback for non-standard files.
- **macOS network discovery not retrying after Local Network permission grant** — On macOS, the first launch triggers a Local Network permission prompt while the mDNS daemon is already browsing. After the user grants permission, the daemon doesn't automatically re-browse. Added periodic re-browse logic (every 30 seconds, up to 3 attempts) that fires only when no servers have been discovered yet, so services are found without requiring an app restart.

### Changed
- **Version bumped to 0.4.0** for the new feature release cycle.

## [0.3.1] — Unreleased (development)

### Added
- **Pedantic Clippy analysis** — Enabled `clippy::pedantic` and `clippy::nursery` crate-wide with curated `#![allow(...)]` for GTK-specific patterns. All 392 initial warnings resolved. Pedantic linting now runs in CI on every push/PR and locally via `build-windows.ps1 -Clippy`, `build-linux.sh --clippy`, and `build-macos.sh --clippy`.
- **`cargo audit` in CI** — New Security Audit job checks `Cargo.lock` against the RustSec Advisory Database on every push and pull request.
- **Code coverage in CI** — `cargo-llvm-cov` generates HTML coverage reports on the Linux x86_64 CI job, uploaded as downloadable artifacts.
- **Weekly fuzz testing** — `cargo-fuzz` target for the DMAP binary protocol parser runs for 5 minutes every Sunday via a scheduled GitHub Actions workflow. Crash artifacts are uploaded automatically.
- **Comprehensive unit test suite** — Added 60+ new test functions (up from 11) covering:
  - `smart_rules.rs` — all text/numeric/date operators, match modes (All/Any), limit by items/duration/size, sort ordering, leap year helper, date cutoff computation.
  - `tag_parser.rs` — `is_audio_file()` for all extensions, case insensitivity, unsupported formats, edge cases.
  - `audio/mod.rs` — `slider_to_pipeline()` curve properties, `redact_url_secrets()` for Plex/Jellyfin/Subsonic tokens, volume path helper.
  - `daap/dmap.rs` — i8/i64 extraction, zero-length strings, nested containers, missing tags, partial headers, max values, UTF-8 strings.
  - `local/engine.rs` — `db_model_to_track()` conversion, None fields, invalid UUID fallback, invalid date handling, `get_mtime()` on nonexistent files.
- **Property-based testing** — `proptest` added as dev-dependency with 3 property tests for the smart rules engine: "All mode ⊆ Any mode", "limiting never increases count", "empty rules returns all tracks".
- **Developer build script flags** — `build-linux.sh` and `build-macos.sh` now support `--check`, `--fmt`, `--clippy`, and `--coverage` quick-exit flags matching the existing Windows `build-windows.ps1 -Check/-Fmt/-Clippy/-Coverage` pattern.
- **Local coverage reporting** — All three build scripts support `--coverage` / `-Coverage` to run `cargo llvm-cov --summary-only` with auto-install. Coverage reports exclude untestable modules (UI, remote backends, DB migrations) via `--ignore-filename-regex` for meaningful percentages.
- **DMAP parser tag coverage** — Added `mikd` (media item kind, I8) and `mper` (persistent ID, I64) to the DMAP tag classification table.

### Fixed
- **Windows Winget/Add-Remove Programs showing version in app name** — Added `AppVerName=Tributary` to the Inno Setup script so the display name is just "Tributary" instead of "Tributary version 0.3.0" in Winget upgrade lists and Add/Remove Programs.

### Changed
- **Windows x86_64 toolchain switched from GCC to Clang/LLVM** — The Windows x86_64 build now uses MSYS2's CLANG64 environment (`mingw-w64-clang-x86_64-*` packages) with the `x86_64-pc-windows-gnullvm` Rust target instead of UCRT64/GCC (`x86_64-pc-windows-gnu`). This unifies both Windows architectures (x86_64 and aarch64) on the same LLVM/Clang compiler family, improving toolchain consistency and enabling better LLVM integration for future sanitizer and profiling work. End-user binaries are identical native Windows PE executables.
- **CI Clippy upgraded to pedantic** — The Linux CI Clippy step now uses `cargo clippy --all-targets -- -D warnings` which picks up the crate-level `#![warn(clippy::pedantic, clippy::nursery)]` configuration. macOS and Windows CI continue to use `-D warnings` (same effect since the crate-level warns are active).
- **CI coverage excludes untestable modules** — The `cargo-llvm-cov` step now uses `--ignore-filename-regex` to exclude UI, remote backend clients, DB migrations, desktop integration, and `main.rs` from coverage reports, providing meaningful coverage percentages for testable core logic.

## [0.3.0] — 2026-04-15

### Added
- **Realtime text search filter** — A search bar above the browser panes filters the tracklist in real-time with case-insensitive substring matching across title, artist, album, and genre. Composes with the existing genre/artist/album browser selections. Search text clears automatically on source switch.
- **Song properties dialog** — Right-click any track → "Properties…" opens a dedicated dialog with editable fields (Title, Artist, Album, Genre, Year, Track #, Disc #) and read-only info (Format, Bitrate, Sample Rate, Duration, File Path). Tags are written safely via `lofty` with explicit Save/Cancel buttons — no inline editing in the tracklist. Supports MP3 (ID3v2), M4A/AAC (MP4 atoms), OGG Vorbis, and FLAC.
- **Batch metadata editing** — When multiple tracks are selected, the Properties dialog shows only batch-appropriate fields (Artist, Album, Genre, Year, Disc #). Fields with mixed values display a "Mixed" placeholder. Only user-modified fields are written; unchanged fields are left untouched per-file.
- **MusicBrainz auto-fill** — Single-track Properties dialog includes a "MusicBrainz Lookup" button that queries the MusicBrainz Recording Search API by title + artist. Results populate the form fields but still require manual Save — no automatic writes.
- **State/Province column for Internet Radio** — Radio stations now display a "State/Province" column (repurposing the Album column) alongside the existing Country column. The Radio-Browser API `state` field is populated for most US and international stations.
- **Tiered "Stations Near Me" search** — Near Me now uses a three-tier search strategy: (1) geo-distance sorted stations with real coordinates, (2) stations matching the user's state/province (catches stations like WBAA that have state metadata but no coordinates), (3) country-only fallback sorted by votes. Results are merged and deduplicated, with nearby stations appearing first.
- **Column drag-and-drop reordering** — Tracklist columns can now be reordered by dragging their headers. Column order is persisted to `config.json` and restored on launch.
- **Local library playlists** — Regular and smart playlists for the local library backend. Playlists survive library folder changes via fingerprint-based track matching (title, artist, album, duration). Smart playlists support iTunes-style rules with filterable metadata fields, text/numeric/date operators, result limiting, and reevaluation against the current library when opened or exported.
- **Fedora COPR installation instructions** — README now documents the `jmsqrd/tributary` COPR repository for one-command Fedora installation.
- **Arch Linux AUR installation instructions** — README now documents the three AUR packages (`tributary`, `tributary-bin`, `tributary-git`).
- **Windows winget installation instructions** — README now documents `winget install jm2.Tributary`.
- **Flathub readiness** — AppStream metainfo now includes `<developer>` tag, `<screenshots>` section, and full release history. Flatpak manifest updated with `xdg-music:rw` permission for tag editing.

### Performance
- **Batch tracklist updates via `splice()`** — `display_tracks()` now uses `ListStore::splice()` to replace all items in a single atomic operation instead of `remove_all()` + N individual `append()` calls. This eliminates multi-second UI freezes when connecting to large remote libraries (e.g., DAAP with thousands of tracks) or switching between sources. The sidebar connection spinner now animates smoothly during the transition.
- **Persistent album art worker thread** — Remote album art fetching now uses a single long-lived background thread with a reusable HTTP client instead of spawning a new `std::thread` (and new TLS session) for every track change. A generation counter discards stale fetches when the user rapidly skips through tracks, eliminating UI lag on remote shares.
- **Shared database connection** — Playlist operations (load, create, rename, delete, add/remove tracks, smart rule editing) now reuse a single SQLite connection via `tokio::sync::OnceCell` instead of opening a new connection and re-running migrations on every action. Eliminates ~9 redundant `init_db()` calls per session.
- **Debounced browser rebuilds during scan** — `TrackUpserted` and `TrackRemoved` events now defer the 3-pane browser rebuild by 500 ms using a generation-counter timer. During initial library scan with many files, this collapses dozens of consecutive rebuilds into a single update, preventing UI lag. The tracklist store is still updated immediately so tracks appear in real-time.
- **Search filter debounce (100 ms)** — The browser search entry now debounces filter callbacks by 100 ms using a generation-counter timer, preventing the expensive filter-and-rebuild from firing on every keystroke during fast typing.

### Fixed
- **About dialog icon missing on macOS `.app` bundles** — The macOS build script now copies the app's hicolor icons (`data/icons/hicolor/`) into the `.app` bundle's `Contents/Resources/share/icons/hicolor/`. Additionally, the icon theme search path now includes the bundle's `Contents/Resources/share/icons` directory at runtime.
- **M4A/MP4 album art not displaying on macOS** — Added a raw MP4 `covr` atom parser as a last-resort fallback when lofty's unified `pictures()` API doesn't surface embedded artwork. This handles Apple-encoded M4A files where the cover art is stored in iTunes-style atoms that lofty's tag abstraction doesn't expose on all platforms.
- **DAAP remote tracks missing album art** — The DAAP backend now constructs per-track cover art URLs (`/databases/{db}/items/{id}/extra_data/artwork`) and sets `cover_art_url` on each track. Previously all DAAP tracks had `cover_art_url: None`, so the header bar always showed the generic placeholder icon.
- **Windows ARM64 build script suggesting x64 MSYS2 packages** — `build-windows.ps1` now auto-detects ARM64 via `RuntimeInformation.ProcessArchitecture` and `PROCESSOR_ARCHITECTURE`, defaulting to `aarch64-pc-windows-gnu` / `clangarm64` instead of always suggesting x64 packages.
- **Blank window on Fedora WSL (Vulkan/dzn driver incompatibility)** — Added WSL detection in `main.rs` (checks `WSL_DISTRO_NAME`, `WSL_INTEROP`, and `/proc/sys/fs/binfmt_misc/WSLInterop`) that forces `GSK_RENDERER=gl` to avoid the broken Vulkan Dozen (dzn) driver path in WSLg.
- **SHA256 checksums only contained Flatpak artifacts** — The release workflow's `upload-artifact` steps were conditional on `workflow_dispatch`, so during `release` events the checksums job couldn't find non-Flatpak artifacts. Made artifact uploads unconditional for all platform jobs (macOS, Windows, DEB, RPM, Arch).
- **Location consent dialog referenced wrong service** — The geolocation consent dialog said "ip-api.com" but the app actually uses a multi-provider HTTPS cascade (`ipapi.co`, `ipwho.is`, `freeipapi.com`). Updated to generic "a geolocation service" text.
- **"Reset to Defaults" only reset column visibility** — The Preferences reset button now also resets column ordering to the default layout.
- **Inno Setup post-install dialog could hang silent installs** — The `[Run]` section used `skipifsilent`, which still showed the "Launch Tributary" checkbox under `/SILENT` (only suppressed under `/VERYSILENT`). Changed to `skipifnotsilent` so the dialog is never shown during any silent install, preventing automation pipeline hangs (e.g. winget).
- **Context menu "Add to Playlist" showed scrollbars** — GTK4's `PopoverMenu::from_model()` wraps submenu sections in an internal `ScrolledWindow` that added unnecessary scrollbars even for menus with only 2–3 entries. Replaced the `append_section` submenu approach with a flat menu structure: a disabled "Add to Playlist" header item followed by indented playlist names as regular clickable items. No submenu = no internal ScrolledWindow = no scrollbars. Previous CSS-only and programmatic `ScrolledWindow` traversal fixes were insufficient.
- **Right-click context menu selected wrong row** — The right-click handler used a hardcoded 25px row height estimate to guess which row was clicked, but didn't account for the column header height, causing consistent off-by-one selection errors. Removed the unreliable row-estimation block entirely — the context menu now operates on whatever row(s) are already selected via normal left-click.

### Changed
- **Windows GSK renderer switched from Vulkan to GL** — The Windows renderer override was changed from `GSK_RENDERER=vulkan` to `GSK_RENDERER=gl` for broader driver compatibility. The GL renderer still provides hardware-accelerated libadwaita animations but avoids Vulkan driver issues on some systems.
- **Geolocation now extracts region/state** — All three geolocation providers (`ipapi.co`, `ipwho.is`, `freeipapi.com`) now return `region` data, enabling state-level radio station filtering.
- **Radio column layout expanded** — Radio mode now shows 6 columns (Title, Country, State/Province, Tags, Bitrate, Format) instead of 5.
- **Browser layout restructured** — The browser is now a vertical Box containing the search entry on top and the horizontal genre/artist/album panes below. `rebuild_browser_data` and `update_browser_visibility` updated accordingly.

### Removed
- **Dead `platform` module** — The Phase 1 stub `src/platform/mod.rs` (with its own `MediaControlsBackend` trait and no-op implementations) was never used and has been superseded by `src/desktop_integration/mod.rs` (souvlaki). Deleted the module and removed `mod platform` from `main.rs`.

## [0.2.1] — 2026-04-05

### Fixed
- **DAAP date modified 31 years in the future** — The DAAP backend was adding a 978,307,200-second epoch offset to `asdm` (song date modified) values, assuming they were relative to the DAAP epoch (2001-01-01). In practice, real-world DAAP servers (forked-daapd, OwnTone, etc.) send standard Unix timestamps. Removed the offset so dates display correctly. No other protocols were affected.
- **Avahi duplicate hostname discovery** — When Avahi services are freshly registered (rather than available at startup), Avahi's conflict resolution can append `-2`, `-3`, etc. to hostnames. The mDNS discovery now strips these suffixes before building the dedup key, preventing duplicate sidebar entries like `myhost-2`.
- **Preferences checkmarks low resolution on Windows/Linux** — The CSS selector was targeting the wrong GTK4 node (`checkbutton indicator` instead of `checkbutton check`). Fixed the selector to target the correct sub-node for crisp rendering across all platforms. Grid layout preserved.
- **Inno Setup installer not fully silent for Winget** — Added `AppId` (stable GUID), `VersionInfoVersion`, `PrivilegesRequiredOverridesAllowed=commandline`, `DisableDirPage=auto`, `DisableProgramGroupPage=auto`, `CloseApplications=yes`, `CloseApplicationsFilter=tributary.exe`, and `SetupLogging=yes` to the Inno Setup script. These directives ensure Winget's `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-` flags work correctly for fully unattended installs.
- **About Tributary icon missing on Windows and Linux (RPM)** — The About dialog referenced icon name `"tributary"` but the hicolor theme files are named `io.github.tributary.Tributary.png`. Changed the icon name to `io.github.tributary.Tributary` and updated the icon theme search paths to look in `data/icons/` (development) and `share/icons/` (installed). The Windows build script now bundles the app's hicolor icons into the dist folder.
- **Failed server connection doesn't revert sidebar selection** — When a server connection attempt fails (timeout, bad credentials, etc.), the sidebar now reverts to the previously selected source instead of staying on the failed entry. A connection guard tracks the pending connection URL and pre-connection sidebar position, clearing the spinner and restoring selection on failure.
- **Remote cover art not loading** — `fetch_remote_album_art()` relied on `tokio::runtime::Handle::try_current()` which silently fails on the GTK main thread (no tokio context). Replaced with a `std::thread` + `reqwest::blocking::Client` pattern (matching the local album art extractor), with a 15-second timeout. No auth tokens are logged.
- **Radio station elapsed time not updating** — The GStreamer position timer required both position and duration to be queryable before emitting `PositionChanged` events. Live radio streams have no finite duration, so position ticks were never sent. Now emits ticks with `duration_ms: 0` for live streams. The UI shows elapsed time and displays "LIVE" instead of a duration, and the progress slider stays inactive. This also fixes the occasional endless buffering spinner on slow-to-load streams, since position ticks now clear the spinner for live streams too.

## [0.2.0] — 2026-04-04

### Added
- **Internet Radio** — New "Internet Radio" sidebar section with three sub-sources:
  - **Top Clicked** — most-clicked stations from the Radio-Browser community database.
  - **Top Voted** — highest-rated stations.
  - **Stations Near Me** — geo-located stations sorted by distance, with user consent prompt for IP-based geolocation (via `ipapi.co` over HTTPS).
  - Dynamic column switching: radio sources show Title, Country, Tags, Bitrate, Codec; music sources restore the full 12-column layout.
  - Browser panes are hidden when viewing radio stations.
  - DNS-based Radio-Browser API mirror resolution (`all.api.radio-browser.info`).
  - Double-click plays the station's stream URL via GStreamer.
- **Manual server addition** — `+` button in the sidebar toolbar opens an "Add Server" dialog with server type dropdown (Subsonic, Jellyfin, Plex), URL, username, and password fields. Servers are persisted to `servers.json` (type, name, URL only — no credentials stored).
- **Manual server deletion** — Trash button on manually-added servers removes them from the sidebar and `servers.json`.
- **Regular discovery refresh** — mDNS `ServiceRemoved` events now remove offline servers from the sidebar. Jellyfin UDP discovery re-broadcasts every 60 seconds; servers are removed after 3 consecutive missed cycles (3 minutes).
- **`DiscoveryEvent` enum** — Discovery channel now carries `Found` and `Lost` variants instead of raw `DiscoveredServer` structs.
- **`manually_added` field** on `SourceObject` — distinguishes user-added servers from auto-discovered ones. Manually-added servers are never auto-removed by discovery refresh.
- **`location_enabled` preference** — Persisted in `config.json` to remember the user's geolocation consent choice.
- **About dialog** — Ptyxis-style `adw::AboutDialog` with app icon, version, author (John-Michael Mulesa), website link to GitHub, and "Report an Issue" link to GitHub Issues.
- **SHA256 checksums** — CI and Release workflows now generate and upload `SHA256SUMS.txt` for all build artifacts.
- **Packit / Fedora COPR** — RPM spec and `.packit.yaml` for automated Fedora COPR builds.

### Changed
- **Sidebar** — Now returns a `gtk::Box` (scrolled list + toolbar) instead of a bare `ScrolledWindow`. The toolbar contains the `+` add-server button.
- **Sidebar categories** — Jellyfin and Plex servers now appear under separate "Jellyfin" and "Plex" sidebar headers instead of the combined "Jellyfin / Plex" category. "Internet Radio" added as the last category.
- **Buffering spinner** — Debounce threshold increased from 100 ms to 300 ms to prevent sub-100 ms blinking on fast-loading local files. Added `PositionChanged`-based fallback: if the elapsed time is advancing (audio is actually playing), the spinner is cleared definitively — fixing the endless spinner bug on some remote streams where GStreamer never emits a clean `Playing` state transition after buffering.
- **Jellyfin UDP discovery** — Now runs in a continuous loop (60-second intervals) instead of a single broadcast, enabling dynamic server detection and removal.

### Security
- **HTTPS geolocation** — Multi-provider HTTPS cascade (`ipapi.co` → `ipwho.is` → `freeipapi.com`) for IP-based geolocation. All providers are reputable, global, and require no API keys.
- **Request timeouts** — All Radio-Browser API and geolocation HTTP requests now have a 15-second timeout to prevent indefinite hangs. DAAP logout requests now use a 5-second timeout.
- **Stream URL validation** — Radio station stream URLs are filtered to only allow `http://` and `https://` schemes, preventing `file://` or other scheme injection from malicious Radio-Browser entries.
- **Auth token redaction in logs** — Added `redact_url_secrets()` utility that masks `X-Plex-Token`, `api_key` (Jellyfin), and Subsonic `t`/`s` (token/salt) query parameters before they reach log output. Applied to all debug-level request logging in Plex, Subsonic, and Jellyfin clients, and to the GStreamer `load_uri` info log. Subsonic salt (`s`) is only redacted when the token param (`t`) is also present, avoiding false positives on unrelated URLs.
- **SQLite WAL mode** — Enabled `PRAGMA journal_mode=WAL` and `PRAGMA busy_timeout=5000` on database init for safer concurrent access and reduced `SQLITE_BUSY` errors.

### UI Polish
- **Preferences checkmarks** — Added CSS rule to fix low-resolution checkmark indicators on Windows (`checkbutton indicator` forced to 18×18px with explicit icon size).
- **Radio column rename** — The "Artist" column is dynamically renamed to "Country" when viewing radio stations, and restored to "Artist" when switching back to music sources.
- **Stations Near Me accuracy** — Added `has_geo_info=true` filter to Radio-Browser queries so only stations with actual coordinates are returned. Country code from geolocation is passed as `countrycode` filter for dramatically improved local relevance.

### Removed
- **Keyboard Shortcuts** menu item removed from the hamburger menu (was non-functional).

### Fixed
- Endless playback spinner on certain remote source tracks.
- Spinner blinking at sub-100 ms intervals on local file playback.
- `remove_empty_category_header` was previously dead code — now actively used by discovery `Lost` events.
- **Runtime panic in `fetch_remote_album_art`** — `tokio::task::spawn` was called from the GTK main thread which has no tokio runtime context. Replaced with `Handle::try_current()` guard to safely spawn on the background runtime.
- **Album art blocking GTK main thread** — `update_album_art()` now extracts embedded picture bytes on a background `std::thread`, sending results back via `async_channel`. Previously, `lofty::read_from_path()` ran synchronously on the GTK thread, causing UI freezes on large FLAC files.
- **Tokio runtime premature shutdown** — Replaced `tokio::signal::ctrl_c()` parking with `std::future::pending()`. The `ctrl_c` approach was unreliable on Windows without a console and could drop in-flight async tasks if Ctrl+C fired before GTK exited.
- **`DaapBackend::Drop` panic on shutdown** — The `Drop` impl now guards `tokio::task::spawn` with `Handle::try_current()` to avoid panicking when the runtime has already been dropped during process teardown.
- **Double-wrapped LIKE pattern in local search** — `LocalBackend::search()` was passing `%query%` to SeaORM's `.contains()` which adds its own `%` wrapping, producing `%%query%%`. Now passes the raw query string.
- **Unsafe `.unwrap()` on `ColumnViewColumn` downcast** — `restore_sort_state()` now uses `let Some(col) = ... else { continue }` instead of `.unwrap()`.
- **Settings directory not pre-created** — `save_volume()` and all window settings persistence (`save_repeat_mode`, `save_shuffle`, `save_sort_state`) now ensure the `<data_dir>/tributary/` directory exists via `create_dir_all` before writing, preventing silent failures on first launch before the database initialises.

---

## [0.1.0] — 2026-04-02

### Added

#### Core Architecture
- Project skeleton with `MediaBackend` async trait, core data models (`Track`, `Album`, `Artist`, `SearchResults`, `LibraryStats`), and structured `BackendError` types.
- GTK4 / libadwaita application with reverse-DNS ID `io.github.tributary.Tributary`.
- Tokio background runtime for async I/O, bridged to the GTK main thread via `async_channel`.

#### UI
- **Rhythmbox-style layout** — three-pane design with sidebar, browser filter, and tracklist.
- **GtkColumnView tracklist** — 12 resizable, sortable columns: #, Title, Time, Artist, Album, Genre, Year, Date Modified, Bitrate, Sample Rate, Plays, Format.
- **Three-state column sorting** — ascending → descending → none (clear sort), with sort state persisted across launches.
- **Browser filtering** — Genre → Artist → Album cascading filter panes with "All" entries and item counts.
- **Sidebar** — source list with category headers (Local, Subsonic, Jellyfin, Plex, DAAP), discovered server entries with lock/unlock icons, connecting spinners, and DAAP eject (disconnect) button.
- **Header bar** — playback controls (prev/play-pause/next), repeat (off/all/one) and shuffle toggles, now-playing widget with album art + title + artist, progress scrubber with position/duration labels, volume slider with cubic perceptual curve, and hamburger menu.
- **Album art** — extracted from embedded tags (ID3, Vorbis, MP4/M4A `covr` atom) and displayed in the header bar.
- **Status bar** — track count and total duration summary.
- Light & dark mode via libadwaita automatic color scheme.

#### Local Backend
- SQLite persistence via SeaORM with automatic migration.
- `lofty` audio tag parsing for metadata extraction.
- `notify` real-time filesystem watching with debounced event handling.
- Async scanning engine with progress reporting.
- `date_modified` from filesystem mtime, used for incremental re-scan.

#### Remote Backends
- **Subsonic / Navidrome** — REST/JSON client with MD5 token authentication, full library fetch (artists → albums → songs), search, cover art URLs, and authenticated streaming URLs.
- **Jellyfin** — REST/JSON client with API key and username/password authentication, music library discovery, paginated item fetching, cover art and streaming URLs.
- **Plex** — REST/JSON client with X-Plex-Token and plex.tv sign-in authentication, music library section discovery, track/album/artist fetching, thumbnail and streaming URLs.
- **DAAP / iTunes Sharing** — DMAP binary TLV protocol parser (nom-based, 24+ tag types), 5-step HTTP session handshake (server-info → login → update → databases → items), password-only auth dialog, session-id streaming URLs, best-effort logout on disconnect.

#### Discovery
- **mDNS** — zero-config discovery for `_subsonic._tcp`, `_plexmediasvr._tcp`, and `_daap._tcp` services via `mdns-sd`.
- **Jellyfin UDP** — broadcast discovery on port 7359 with JSON response parsing.
- **DAAP password probe** — background `/server-info` check to determine if a share requires authentication (lock icon vs. open icon in sidebar).
- Automatic sidebar population with deduplication.

#### Audio Playback
- GStreamer `playbin3` pipeline (with `playbin` fallback) for local and remote stream playback.
- Play/pause, stop, seek, next/previous track controls.
- Shuffle (random, avoid repeat) and repeat (off/all/one) modes with persistence.
- Volume control with cubic perceptual curve and persistence across launches.
- Buffering state detection with debounced spinner display.
- End-of-stream auto-advance with shuffle/repeat awareness.

#### Desktop Integration
- **MPRIS** (Linux) / **SMTC** (Windows) / **MPNowPlayingInfoCenter** (macOS) via `souvlaki` — play/pause/stop/next/previous media key support, now-playing metadata updates.
- HWND extraction on Windows for SMTC initialization.

#### Packaging & CI
- **Linux** — `.deb` (Debian/Ubuntu), `.rpm` (Fedora), Arch Linux `PKGBUILD`, Flatpak manifest.
- **macOS** — `.app` bundle with rpath-fixed dylibs, ad-hoc code signing, `.dmg` packaging. Runtime environment setup for bundled GTK/GStreamer resources.
- **Windows** — MSYS2 UCRT64 DLL bundling, `.zip` portable package, Inno Setup `.exe` installer. Bundled GStreamer plugin path detection.
- **CI** — GitHub Actions workflows for Linux (x86_64 + aarch64), macOS (aarch64), Windows (x86_64 + aarch64) with clippy, rustfmt, build, and test.
- **Release** — automated release workflow producing Flatpak, DMG, Windows zip + installer, deb, rpm, and Arch packages for all supported architectures.

#### Other
- Custom CSS stylesheet for data-table styling and album art placeholder.
- App icon (PNG + ICO + macOS iconset).
- `.desktop` file and AppStream metainfo for Linux desktop integration.
- Windows resource file with icon embedding.

[Unreleased]: https://github.com/jm2/tributary/compare/v0.5.1...HEAD
[0.5.1]: https://github.com/jm2/tributary/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/jm2/tributary/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/jm2/tributary/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/jm2/tributary/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/jm2/tributary/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jm2/tributary/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/jm2/tributary/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jm2/tributary/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jm2/tributary/releases/tag/v0.1.0
