# Source-scoped regular playlist storage, authority, and UI contract

This document defines the durable-storage contract, live-catalogue authority, and mixed-source UI
integration for [P1.5](task.md#p15--persist-source-scoped-playlists). The storage foundation landed
in [#140](https://github.com/jm2/tributary/pull/140), Record A's default-deny live authority in
[#141](https://github.com/jm2/tributary/pull/141), and Record B's Add/Remove/render/Play consumer in
[#142](https://github.com/jm2/tributary/pull/142).

The central rule is:

> A regular-playlist occurrence identifies media by the stable pair `(SourceId, TrackId)`. A
> playlist is an ordered view over media owned by those sources; it is not itself a media source.

That pair is identity, not location. A server URL, stream URL, file URI, filesystem path, access
token, password, DAAP session ID, media lease, or source session epoch must never be substituted for
either component or persisted as remote playlist authority.

## Scope and delivery boundary

The [#140](https://github.com/jm2/tributary/pull/140) storage slice covers:

- migration 13 and the `playlist_entries` entity;
- deterministic conversion of every valid existing entry to the built-in local `SourceId`;
- a canonical source-scoped identity plus a separate local foreign-key cache;
- preservation of playlist order, duplicate occurrences, entry identity, and local reconciliation
  evidence;
- typed storage operations for non-local additions without admitting locators or credentials—the
  schema is source-generic, but it is not by itself a promise that every source kind is addable; and
- compatibility for all currently shipping local regular-playlist, XSPF-import, reconciliation,
  rename, and deletion paths.

At that delivery boundary the storage slice deliberately did **not** enable remote Add to Playlist,
mixed-source rendering or playback, disconnected-row presentation, or playlist-UI lifecycle
refresh. Record A added the internal live-registry authority described below, and Record B now
integrates it into those user-facing behaviors without changing the durable schema. Remote XSPF
metadata export still needs an explicit no-locator policy. Subsonic server-native playlist
integration remains a separate capability with its own
[`subsonic-playlist-sync.md`](subsonic-playlist-sync.md) contract. Its bounded read-only protocol and
exact-session read/commit authority, strict link persistence, and atomic import/pull/conflict engine
are implemented without reusing or widening this regular-playlist authority. Record E
[#146](https://github.com/jm2/tributary/pull/146) now also has
typed read-only sidebar state, ordinary-action exclusion, commit-only local CRUD publication, and
the localized recovery-shell plan. A follow-up durable SQLite revision and lifecycle-owned
full-snapshot publisher now order scan seeding, ordinary CRUD, raw/cascade domain-table writes, and
server-link changes; GTK rejects equal or older snapshots. A separate GTK-free coordinator now
orders typed source/remote/local operations, serializes same-key work through admitted settlement,
schedules bounded exact-session reconnect recovery, reports redacted manual completions, and drains
admitted work during shutdown. The browser and visible server-playlist controls remain the final
follow-on slice.

Smart playlists are unaffected. They remain live queries over the local library rather than stored
regular-playlist occurrences.

## Durable schema

Each `playlist_entries` row remains one occurrence. Repeating the same song twice therefore creates
two rows with different entry IDs and positions; media identity is intentionally not unique within
a playlist.

| Column | Contract |
|---|---|
| `id` | Stable identity of this playlist occurrence. |
| `playlist_id` | Owning playlist; deletion of the playlist cascades to its entries. |
| `position` | Non-negative ordered occurrence position, unique within the owning playlist. |
| `source_id` | Canonical textual `SourceId` of the media owner. It is never a URL or display key. |
| `track_id` | Exact source-native `TrackId`. It is nullable only for an unmatched local XSPF/import legacy occurrence that has not acquired track identity. |
| `local_track_id` | Nullable local integrity/reconciliation cache with `tracks(id) ON DELETE SET NULL`; it is not a second media identity. |
| `match_title`, `match_artist`, `match_album`, `match_duration_secs` | Non-secret normalized evidence used only to reconcile unmatched entries owned by the built-in local source. A non-local row may retain optional safe fingerprints as non-authoritative context, but they are not display-quality metadata and never authorize matching a different remote track. |
| `match_file_path` | Optional exact local path evidence supplied by XSPF import. It is permitted only for the built-in local source and never becomes remote playback authority. |

The following invariants apply:

1. `source_id` is always a canonical lowercase, non-nil UUID. Existing and newly imported local
   occurrences use `SourceId::local()` exactly.
2. A populated `local_track_id` is valid only for the local source and equals that entry's
   `track_id`. Its foreign key preserves the existing automatic orphan transition when a local
   track disappears.
3. A non-local entry has a non-empty `track_id` of at most 4,096 UTF-8 bytes, has no
   `local_track_id`, and has no `match_file_path`. Local identity retains the architecture's larger
   262,144-byte legacy ceiling.
4. A missing `track_id` is valid only for an unmatched local entry with usable reconciliation
   evidence: either a nonblank path or both a nonblank normalized title and artist. It is not an
   invitation to choose a similarly named track from another source.
5. `(playlist_id, position)` remains unique, while `(source_id, track_id)` deliberately does not:
   duplicates are ordered occurrences, not corruption.
6. Schema constraints and typed storage reads fail closed on inconsistent rows. A malformed source
   identity, mismatched local cache, or non-local path is never reinterpreted as local media and
   never passed to a source adapter.

The optional local foreign key is deliberately separate from the canonical pair. Keeping it
retains SQLite's `ON DELETE SET NULL` integrity for local-library churn without forcing a remote
track ID to reference the unrelated local `tracks` table. A local deletion can therefore make an
entry unavailable for reconciliation while preserving the occurrence, its source-scoped identity,
position, and fingerprint evidence.

## Migration 13

Migration 13 transactionally rebuilds `playlist_entries` because SQLite cannot transform the old
local-only foreign-key layout in place while preserving its constraints and indexes.

For every valid predecessor row it:

1. preserves `id`, `playlist_id`, `position`, all `match_*` evidence, and the old nullable
   `track_id` byte-for-byte;
2. writes the exact built-in local source identity to `source_id`; and
3. copies the old linked `track_id` into `local_track_id`, leaving both identity fields null for an
   already-unmatched local entry.

No metadata or path heuristic participates in migration. The migration neither resolves an orphan
nor invents a new track ID. Existing order and repeated occurrences survive even when multiple rows
name the same local track. A legacy row with no track identity and no usable path or normalized
title-and-artist evidence is corrupt rather than an unmatched import, so the migration rejects it.
All table creation, copying, constraint/index restoration, and foreign-key validation occur in one
transaction; any error restores the complete predecessor schema and data so the upgrade remains
retryable.

The reverse migration is exact or it does not run. It can rebuild the predecessor schema while all
rows are representable as local entries, but it transactionally refuses a non-local or otherwise
unrepresentable row. It never discards such an occurrence, converts it to an unmatched local row,
or retargets it by metadata; refusal leaves the target schema and all data intact.

The migrator recognizes only the expected predecessor and target table definitions, foreign keys,
primary key, and standard index shapes. Existing additional explicit indexes are captured and
restored, but a partial or near-match standard schema is an error rather than a reason to guess
which columns or constraints are safe to keep. This protects interrupted upgrades and prevents a
later appended column from making a valid migrated table look like the legacy definition.

## Storage operations and local compatibility

The storage boundary accepts typed `SourceId` and `TrackId` values. A non-local insertion may retain
optional non-secret metadata fingerprints, must not provide a file path, and must commit all
selected occurrences atomically. Those normalized snapshots are neither display fields, identity,
nor matching authority. The exact same native track ID from two sources remains two different
media objects.

The current user-visible behavior is:

- **All-or-none Add:** a local selection writes `(SourceId::local(), tracks.id)` plus the matching
  `local_track_id`. An authenticated Subsonic, Jellyfin, Plex, or DAAP selection must first resolve
  every ordered `MediaKey` through Record A. After staging the ordered SQL inserts and immediately
  before commit, the same transaction revalidates the complete result and atomically acquires exact
  current session/catalogue permits. A result made stale during staging rolls the transaction back;
  after admission, refresh, replacement, disconnect, and shutdown wait for commit or rollback.
  The transaction and permits transfer to an independent completion worker before that final wait,
  so caller cancellation or a synchronous lifecycle revoker cannot strand authority or starve the
  commit.
  A current unsupported, disconnected, missing, or invalid-catalogue selection likewise writes
  nothing and presents fixed localized copy. Duplicate selections create distinct ordered
  occurrence IDs.
- **Regular-playlist load:** reads every stored occurrence in position order. Local identities use
  exact current database rows; eligible non-local identities consume only the registry's current
  sanitized metadata. A missing local track, unavailable or retired source, unsupported owner,
  invalid catalogue, or missing remote track becomes an explicit localized unavailable row that
  remains visible and removable. Stale projected work or results are discarded; the playlist is
  invalidated and projected again from current authority rather than presenting staleness as a row
  reason. Persisted fingerprints are never displayed as stale metadata or used to choose a
  replacement.
- **Exact Remove:** removes the selected durable entry IDs in one transaction. Each repeated
  occurrence is independently addressable, unavailable rows require no live source authority, and
  an error leaves every selected occurrence unchanged.
- **Per-occurrence Play and artwork:** the playlist remains a `ViewOrigin`, while each projected
  row and queue item retains its actual media-owning source. Local media uses current retained
  root/file authority. Remote stream and artwork access revalidate the row's exact transient guard
  before and after adapter work, so refresh, replacement, retirement, disconnect, shutdown, or a
  stale epoch/generation cannot reuse a cached locator.
- **XSPF import:** continues to create local-owned entries only. Exact file-path and deterministic
  metadata matching are unchanged, and an unmatched usable row remains a local occurrence with no
  `track_id` or `local_track_id` until reconciliation succeeds.
- **Local reconciliation:** considers only entries whose `source_id` is exactly the built-in local
  source and whose `local_track_id` is absent. It may update both local track fields after the
  existing exact path/fingerprint resolver commits a unique match. It never metadata-matches a
  remote entry.
- **Local track deletion:** the foreign key clears `local_track_id` but preserves the playlist
  occurrence and its reconciliation evidence. Deleting a playlist still cascades its entries.
- **Rename and root reauthorization:** ID-preserving operations retain both source-scoped identity
  and the local cache. Existing guarded path-evidence relocation remains local-only.
- **XSPF export:** refuses a regular playlist containing any remote or unresolved occurrence before
  touching the destination; it never emits a truncated local-only subset. Mixed-source metadata
  export is explicitly deferred until it has a policy that cannot obtain or serialize a protected
  remote locator.

The existing P1.2 refusal remains the correct fail-closed result, but Record B narrows it to a
selection that Record A does not authorize. Retained authenticated Subsonic, Jellyfin, Plex, and
DAAP catalogues may opt in through the explicit capability. Radio-Browser, removable media,
ephemeral external files, and unknown sources remain unsupported; a generic storage shape, backend
label, source key, cached GTK row, or persisted fingerprint cannot authorize them.

## Live registry and accepted-catalogue authority

P1.5 Record A ([#141](https://github.com/jm2/tributary/pull/141)) establishes the internal
authority boundary between durable source-scoped identity and Record B's playlist UI integration
([#142](https://github.com/jm2/tributary/pull/142)).
`ManagedSourceAdapter` exposes a closed
`RegularPlaylistCapability`: its default is `Unsupported`, and only the retained authenticated
Subsonic, Jellyfin, Plex, and DAAP catalogue adapters explicitly opt into
`SourceScopedEntries`. Radio-Browser, removable media, ephemeral external files, the trait default,
and unknown source kinds remain unsupported. Backend names, source provenance, persisted row shape,
and the mere existence of a catalogue never imply support.

One accepted source payload freezes the advertised capability with its complete catalogue.
`resolve_regular_playlist_tracks(&[MediaKey])` returns exactly one ordered
`RegularPlaylistTrackResolution` for each requested occurrence. An authority lookup must match all
of the following exactly:

1. the requested `SourceId` is the current registered source;
2. the non-secret session epoch is still the accepted session;
3. the accepted catalogue generation is still current;
4. the adapter capability is `SourceScopedEntries`; and
5. for each requested occurrence, its native `TrackId` is canonical and present in the accepted
   catalogue's unique native-ID mapping.

An otherwise accepted catalogue with a missing or duplicate catalogue-native identity receives an
`Invalid` regular-playlist authority index. Every requested occurrence for that source then fails
closed as `InvalidCatalogue`, while the catalogue can remain available to existing non-playlist UI
and no duplicate is chosen. Repeated requested IDs remain valid ordered duplicate occurrences and
produce repeated ordered results when the index is valid. Lookup returns exactly one Available or
fixed-reason Unavailable result per requested occurrence: a missing member fails only that
occurrence, while an unsupported source or unavailable session marks its affected occurrences
without erasing valid neighbors from another source. Revalidation separately rejects results made
stale by retirement, replacement, refresh, release, or shutdown.

An Available result exposes only its exact `media_key()`, transient `guard()`, and sanitized
`metadata()`. An Unavailable result exposes its `media_key()` plus one closed reason:
`SourceUnavailable`, `UnsupportedSource`, `InvalidCatalogue`, or `TrackMissing`—never a backend
error. `are_regular_playlist_tracks_current` rechecks a previously returned ordered result against
the current authority state without trusting its metadata copy.

Successful lookup returns a dedicated, whitelisted `RegularPlaylistTrackMetadata` value rather
than cloning `Track`. Only the display, sort, rating, and history fields named by that DTO cross the
boundary; paths, file and network URLs, stream/artwork locators, credentials, leases, routes, and
raw backend errors cannot be inherited accidentally when `Track` grows a field. The closed guard
also carries its non-secret session epoch and accepted catalogue generation transiently; neither
value is persisted in `playlist_entries`. Lookup does not write those entries, mint playback
authority, or authorize a source-native playlist operation.

Projection from the captured immutable catalogue `Arc` runs outside the lifecycle mutex. Before a
lookup returns or guarded adapter work begins, the registry reacquires the mutex and requires the
same exact `Arc` identity, source, epoch, generation, catalogue authority, and active session
lease. A selector can therefore re-enter registry code without deadlock, while a refresh,
replacement, or disconnect completed during selection makes the result unavailable and prevents
stale adapter work.

`resolve_regular_playlist_stream(guard, TrackId)` and
`resolve_regular_playlist_artwork(guard, TrackId)` independently revalidate exact source
membership, capability, session epoch, and catalogue generation before adapter work and again after
the asynchronous result returns. They discard raw adapter failures at the registry seam and expose
only `RegularPlaylistMediaError::Unavailable` or a closed `BackendFailure(FailureCategory)`.
Every lifecycle `AcceptedSnapshot` owns a separate generation lease. Replacement, view removal,
disconnect, shutdown, final-handle teardown, and defensive pruning revoke that lease explicitly
before clearing the snapshot, so a parked `Arc` snapshot cannot delay invalidation; a returned
guarded stream or artwork request carries that same authority through consumption. Connecting or a
failed replacement can retain the already accepted predecessor and its authority. A successful
replacement or same-session catalogue refresh invalidates guards minted from the previous accepted
payload. Disconnect, shutdown, and final source release synchronously deny new authority before
asynchronous teardown can finish.

Record A did not by itself authorize a UI or database operation. Record B now makes these APIs the
only non-local admission and projection authority: Add and rendering consume their ordered result,
then guarded Play and artwork independently revalidate it at use. Remove instead consumes the
durable playlist occurrence ID and therefore remains available even when its media owner is not.
The localized P1.2 refusal still applies whenever the closed capability or current-state checks do
not authorize the complete selected batch.

## Source lifecycle and unavailable identity

Persistent membership does not imply a live source session. Only `source_id` and `track_id` survive
restart; a session epoch is deliberately transient.

Record B validates every non-local identity against the exact current `SourceRegistry` source,
epoch, accepted catalogue generation, capability, and native track. A currently unavailable or
retired source, unsupported source, invalid catalogue, or missing server-side track makes the
occurrence visibly unavailable while leaving its durable row and position intact; a missing or
unmatched local identity follows the corresponding local unavailable path. The unavailable
projection uses fixed localized state, not a persisted fingerprint or stale metadata snapshot, and
remains removable by exact entry ID. Refresh, session replacement, or any other authority change
instead discards stale projection work/results, invalidates the playlist, and projects it again.
Reconnection may restore it only when the same `SourceId` publishes the same `TrackId`; endpoint
similarity and metadata are not identity evidence.

An available row carries its actual media-owning source separately from the playlist view. Its
queue item adopts only the guard returned for that current projection. Source refresh or retirement
invalidates active and cached playlist projections, while at-use stream and artwork checks prevent
an already queued item carrying a stale guard from crossing the lifecycle boundary.

## Ratings and playback history

These records do not transfer ownership of track metadata:

- Tributary ratings remain writable only for tracks owned by the local library. A remote playlist
  row displays the current live source's read-only or unsupported rating capability; this schema
  persists no rating snapshot and grants no write authority.
- Playback history remains local-only. A local occurrence reached through a regular playlist can
  still contribute to its exact local track. A remote occurrence does not update the local
  `tracks` table merely because it appears beside local entries or has an equal native ID.

See the [rating contract](ratings.md), [playback-history contract](playback-history.md), and
[source-lifecycle architecture](architecture/source-lifecycle.md) for those independent authority
boundaries.

## Boundary with server-native Subsonic playlists

A regular-playlist occurrence owns only media identity. It does not say that the containing
playlist originated on a server, retain a native playlist ID, or carry synchronization state. The
server-native foundation therefore introduces a separate content-redacted `NativePlaylistId`,
bounded summary/detail snapshot values, and a separate default-deny `PullSnapshots` capability.
Only the authenticated Subsonic adapter opts in. List/detail work is checked against the exact
current source adapter, session epoch, and revocable lease before and after network I/O; it does not
require current music-catalogue membership and does not grant display, stream, artwork, rating, or
history authority for the returned track IDs.

Import Copy and Keep Synced persistence now consume a current endpoint snapshot and write ordinary
canonical `(SourceId, TrackId)` occurrences under this document's schema. Import Copy commits with
no link and is immediately editable. Keep Synced retains a dedicated migration-14 link outside
`playlist_entries`, permits only one mirror per exact source/native playlist identity, and makes
ordinary playlist mutations and reconciliation reject that mirror; reconciliation excludes all
links with a zero-bind database subquery rather than one host parameter per mirror. Pull replacement
still writes the regular entries all-or-none in exact server order, including duplicates and track
IDs absent from the current catalogue. Neither mode persists an endpoint, credential, locator,
route, lease, session epoch, or raw error. Final commit authority is sealed to the exact pull or
absence result that minted it, so authority from another live operation cannot authorize this
snapshot. Neither mode metadata-matches a missing server track. The complete direction, revision,
conflict, offline, server-deletion, and unlink policy is in the server-native contract.

The UI groundwork in [#146](https://github.com/jm2/tributary/pull/146) now consumes playlist
parents and optional links as one ordered typed snapshot. Link
presence wins over the legacy smart flag, invalid link state rejects publication rather than
falling back to an editable playlist, and only the local playlist ID plus durable presentation state
cross into GTK. Structural header/playlist kinds replace translated-name and backend-string
editability tests. Linked mirrors are visibly read-only or conflicted/missing and are omitted from
ordinary Rename, Export, Delete, Edit Smart, Add, and Remove affordances; manager transactions still
enforce the same boundary if presentation becomes stale. A separate localized footer shell is
reserved for sync/recovery state, but remains hidden until user actions are connected. Coordinator
stamps, generations, cancellation, admission guards, native identity, and observed epochs never
enter GTK, and this separate lane grants no Record A catalogue, playback, rating, or history
authority. Migration 15's exact singleton revision and six transaction-local triggers
now cover playlist parents, links, raw writes to those tables, and cascades. One lifecycle-owned
publisher reads a coherent complete redacted join, coalesces post-commit hints, polls for lost
hints, and publishes the first valid snapshot and thereafter only strictly newer snapshots; GTK
replaces or retracts the entire section and ignores equal or older delivery. Partial post-commit
row patches are gone, closing the former reversed scan/CRUD race. This publication lane performs
no server operation and does not widen the storage or live-source authority described above.

## Validation matrix

The storage record is complete only when automated coverage demonstrates:

- fresh migration and exact migration from the released predecessor schema;
- stable local-source backfill for linked and unmatched entries;
- exact preservation of entry IDs, positions (including valid gaps), duplicate occurrences, and
  every fingerprint/path field;
- restoration of the playlist, unique-position, source/track, local-track, and pre-existing custom
  explicit indexes plus both foreign keys;
- transactional rollback and safe retry after a forced rebuild/copy/index failure;
- rejection of malformed predecessor, partial-target, source/track, local-cache, and non-local-path
  states;
- local add, load, XSPF import, reconciliation, deletion, rename, and root-reauthorization behavior
  unchanged through the new schema; and
- non-local storage isolation: identical native IDs from different sources never collide, remote
  deletion/retirement cannot cascade into playlist storage, and no remote locator or credential is
  accepted or serialized.

The storage and authority foundation records do not retroactively claim their consumers. Native
Subsonic link persistence and atomic synchronization now have their own strict migration,
transaction, revision-CAS, lifecycle-permit, drift, missing, unlink/removal, and redaction
validation. Typed read-only sidebar presentation, the localized recovery-shell plan, and durable
full-snapshot sidebar ordering, same-key coordinator, exact-session reconnect scheduler,
post-staging joint admission, bounded fan-out, shutdown drain, and redacted manual completion
facade are now implemented. The server-playlist browser, visible action consumer, accessibility
coverage, and mixed-source XSPF metadata export remain explicitly deferred. Until
a no-locator mixed-source export policy exists,
a regular playlist containing any remote or unresolved occurrence is refused all-or-none before
XSPF touches its destination; the local-only compatibility projection is never exported as a
truncated result.

Record A additionally requires automated coverage for default-deny adapters, the four explicit
authenticated opt-ins, Invalid playlist indexing for missing/duplicate catalogue-native identity,
sanitized metadata, stale epoch/generation rejection, predecessor retention during connecting or a
failed replacement, invalidation after replacement/refresh, synchronous disconnect/shutdown/final-
release denial, and pre/post-async stream and artwork revalidation. Ordered lookup must preserve
duplicate requested occurrences and isolate one missing track's Unavailable result from valid
neighbors. Passing those tests alone does not claim Add/Remove/render/Play UI integration.

Record B additionally requires automated coverage that the complete selected Add batch is resolved
and admitted under commit-scoped authority all-or-none; local plus each of the four authenticated
opt-ins are admitted while current radio, removable, external, unknown, unavailable, invalid, and
missing cases write nothing, stale final acquisition rolls back staged writes, and lifecycle
invalidation cannot cross an admitted commit. Projection tests preserve occurrence IDs,
positions, and duplicates; retain every unavailable row without displaying fingerprints; discard
stale projection work/results before current reprojection; and restore only exact reconnected
identity. Remove tests address durable entry IDs atomically, including repeated and unavailable
occurrences. Queue and artwork tests retain per-row media ownership, use the closed guard at use,
and reject refresh, replacement, retirement, stale epoch/generation, and missing membership. Rating
and history regressions prove
that playlist membership grants neither remote rating mutation nor local-history ownership to a
remote occurrence. Every new unavailable and mutation result is non-fallback localized across all
13 shipped catalogues.
