# Source identity and lifecycle ownership

- Status: Accepted design; repository PR/CI acceptance pending; implementation is incomplete
- Decision date: 2026-07-17
- Tracker: [P3.1](../task.md#p31-introduce-a-sourcesession-registry)
- Review finding: [Architectural assessment](../../CODE_REVIEW_2026-07-10.md#architectural-assessment)

## Context

Tributary presents local files, playlists, remote libraries, radio stations, removable media, and
files opened by the operating system through one browser and playback surface. Those sources do
not currently share one identity or lifecycle boundary:

- local, playlist, radio, and remote navigation use strings assembled by the UI;
- a remote source's configured URL is also its source key;
- standard authenticated backends and DAAP use separate process-owned registries;
- local, playlist, removable, radio, and external-file queue entries retain a concrete URI;
- remote catalogue identifiers are converted to UUIDv5 values before entering the generic model;
  and
- connection, refresh, cancellation, cached publication, failure, and disconnect behavior is
  split between the GTK window, source-selection callbacks, the local engine, and the registries.

Several important foundations already exist. Playback uses an immutable queue addressed by source
and track identity instead of a mutable GTK row index. Standard remote backends retain private
native locators behind generation-owned, revocable leases, while DAAP retains its stateful session
and performs explicit logout. Source navigation rejects stale asynchronous publications. Local
renames preserve database track IDs, and removable scans are generation-owned and cancelled on
relocation or removal.

This decision defines how those foundations converge. It does **not** claim that the convergence is
implemented, and it does not make the current `MediaBackend` trait the integration seam; that
separate work remains [P3.2](../task.md#p32-make-the-backend-abstraction-real-and-stable).

## Decision

### Identity is typed and location-independent

The application identity of playable media is the pair:

```text
MediaKey = (SourceId, TrackId)
```

`SourceId` is an opaque UUID newtype. `TrackId` is an opaque, non-empty backend-native string
newtype; it is not required to parse as a UUID. Neither type is a display name, URL, filesystem
path, credential, session key, row position, or GTK object identity. Native track IDs are never
logged verbatim because a remote server controls their contents.

The pair is the identity. A `TrackId` need be unique only within its `SourceId`; the same native ID
from two servers, two backend protocols, or a local database and a server is not a collision. A
concrete stream URL, artwork URL, file path, mount point, credential, and remote session lease are
locators owned by a live source adapter, not identity fields.

A queue item also retains a separate `ViewOrigin` (for example, a playlist ID or radio-feed query)
and occurrence number. That preserves duplicate playlist ordering and GTK re-selection without
pretending that a playlist or query creates another copy of a track.

### Source ID assignment and migration

A checked-in application UUID namespace is used only where a deterministic ID is required. The
namespace and the canonical input strings become data-format constants once implemented; changing
either requires a migration.

| Source kind | `SourceId` rule | Stability boundary |
|---|---|---|
| Local library | UUIDv5 of `builtin:local` | Constant across launches and library-root changes |
| Saved remote source | UUID stored with the saved source record | Constant across reconnects and an explicit endpoint rebind |
| Legacy saved remote source | UUIDv5 of `remote:<backend>:<canonical-base-url>`, persisted before use | Stable upgrade without depending on a partially completed rewrite |
| Unsaved discovered or environment remote | UUIDv5 of the same backend plus canonical base URL | Stable while that logical endpoint is unchanged; an explicit save/rebind gives it a persisted record |
| Radio-Browser | UUIDv5 of `builtin:radio-browser` | Constant across feed queries, refreshes, and launches |
| Removable filesystem | UUIDv5 of the existing opaque GIO logical source key | Stable to the same degree as that key; a changed fallback key intentionally means a different source |
| OS-opened external files | Random UUIDv4 for one open request | Deliberately ephemeral; retired with its one-file session |

Canonical remote base URLs use the existing validated base-URL policy and preserve a meaningful
reverse-proxy path. Scheme and host case, default ports, and the optional trailing empty path
segment are normalized; different non-default ports or non-empty base paths remain different
sources. User-info, query, and fragment data are rejected rather than used in identity.

Saved-source persistence replaces the legacy bare JSON array with this versioned envelope:

```json
{
  "schema_version": 1,
  "servers": [
    {
      "type": "subsonic",
      "name": "Home",
      "url": "https://music.example.test/",
      "source_id": "00000000-0000-0000-0000-000000000000"
    }
  ]
}
```

The reader accepts exactly the legacy array as version 0 or the envelope version it implements;
an unknown version, malformed root, or malformed version-1 identity publishes no saved rows and
leaves the original bytes untouched. It reports only a fixed category and count, never row
contents. On a version-0 upgrade, rows that fail the existing backend/URL validation are removed
with the same count-only warning used today. Every remaining row receives the deterministic legacy
ID above. Duplicate canonical `(backend, base URL)` rows collapse to their first file-order row and
its display name because they describe one logical source. A duplicate canonical endpoint with
different pre-existing IDs, or one `source_id` assigned to different canonical endpoints, instead
quarantines the complete file: no row is published or rewritten until the user explicitly removes
or rebinds the conflicting record. Quarantine is a loader state over the unchanged `servers.json`,
not a second persistence store.

The complete version-1 envelope is written to a same-directory temporary file and atomically
replaces the legacy file before any migrated row is published. A write or replacement failure
publishes nothing and preserves the last complete file. Changing an endpoint through a future
explicit rebind retains the stored ID. Merely discovering a different URL never migrates identity
by name or library similarity.

The existing local `tracks.id` value is already the local backend's native `TrackId` and is kept
byte-for-byte, including a legacy non-UUID string. The current random fallback in
`db_model_to_track` is removed rather than rewriting such a row on every read. New local tracks may
continue to receive UUIDv4 strings, and playlist foreign keys remain unchanged.

No remote catalogue or playback queue is persisted today. Therefore remote migration discards and
refetches process-local catalogues; it never attempts to reverse the current one-way UUIDv5 mapping
back into a server ID. If a future persisted format contains only that derived UUID and not the
native locator, the row is invalidated and resynchronized rather than guessed.

Backend-native `TrackId` values are represented as follows:

| Adapter | Native track identity |
|---|---|
| Local and playlist projection | Exact SQLite `tracks.id` string |
| Subsonic | Exact song `id` returned by the server |
| Jellyfin | Exact audio item `Id` returned by the server |
| Plex | Exact track `ratingKey`; the media-part key remains a locator |
| DAAP | Canonical decimal `miid` within the DAAP source |
| Radio | Exact Radio-Browser `stationuuid` |
| Removable media | Adapter-owned opaque relative-file key within the logical filesystem; the absolute mount path is never part of the key |
| External file | Random per-open track ID retained by the ephemeral adapter |

Remote IDs are preserved exactly—without trimming, case folding, URL encoding, or global UUID
hashing—after applying a documented ingestion size bound. An empty or over-limit ID rejects that
catalogue item. The removable adapter may initially use a lossless encoded relative path as its
native key because no platform-independent file ID is available; rename stability on a device is
not promised. Its current source generation maps that key to a validated locator beneath the live
mount. A stronger device-native file ID can replace that representation later without exposing an
absolute path to generic state.

Tributary treats a provider's native ID as authoritative only inside that source. If a server
itself reuses or changes an ID, no metadata heuristic silently repairs the identity; a refresh
publishes the provider's current mapping, and an explicit source rebind remains the boundary for a
different logical server.

### One registry owns source lifecycle

One application-owned registry is authoritative for every `SourceId`. It stores a deliberate
`SourceSession`/resolver abstraction, not necessarily `Arc<dyn MediaBackend>`. That distinction
allows local, radio, removable, and external-file adapters to participate before P3.2 finishes the
browsing trait.

The externally visible state machine is:

```text
                       connect / retry
  Dormant or Failed  -------------------->  Connecting
       ^                                      |     |
       |                     success          |     | failure, no predecessor
       |                 +--------------------+     v
       |                 v                        Failed
       |               Ready  <-------------+
       |                 |  ^                |
       |         refresh |  | success or     | failed/cancelled refresh
       |                 v  | unchanged data |
       |              Refreshing ------------+
       |                 |
       | disconnect, loss, removal, or shutdown
       |                 v
       +----------- Disconnecting ----------> Retired
                         | explicit retained row
                         +-------------------> Dormant
```

The states have these ownership rules:

- `Dormant` has a registered identity but no usable session. A saved row or built-in source can
  remain dormant; an ephemeral discovered/external source may instead be retired.
- `Connecting` owns an operation generation and cancellation token. A reconnect may retain a
  still-valid predecessor session until the replacement succeeds. Replacement success swaps the
  session atomically and then revokes/retires the predecessor; replacement failure restores the
  predecessor as `Ready` with a sanitized failure annotation.
- `Ready` owns exactly one session epoch, source adapter, revocation lease, and the last complete
  catalogue snapshot or set of named view snapshots.
- `Refreshing` keeps that same usable session and each affected view's last complete snapshot while
  one or more bounded refresh lanes are active. Refresh failure returns that lane to its old
  snapshot with a failure annotation; an empty successful snapshot is a real successful result.
- `Failed` has no usable session and retains only a closed failure category and retry policy. It
  never retains a backend error chain, response body, URL, credential, native ID, or local path.
- `Disconnecting` owns the retiring session until its bounded asynchronous close completes. Its
  lease is revoked synchronously on entry, so no new media request or publication can use it.
- `Retired` accepts no work or publication. Shutdown closes the registry gate before asynchronous
  teardown begins.

Each source has two monotonically increasing counters:

1. an **operation generation**, minted before every connect or refresh task is queued and recorded
   as latest for its operation lane (`session`, source-wide catalogue, or exact `ViewOrigin`); and
2. a **session epoch**, adopted only when a connected adapter becomes `Ready`.

Every asynchronous result carries `SourceId`, operation generation, and, when applicable, the
session epoch and `ViewOrigin`. The registry rechecks all fields before changing state or
publishing a snapshot. Starting a newer operation cancels and supersedes the older operation in the
same lane without recording a user failure; independent radio-feed view refreshes may coexist and
cannot overwrite each other. Disconnect, discovery loss, source removal, and shutdown cancel all
lanes and revoke the current session lease synchronously. Backends must cooperate with
cancellation at every bounded network/page boundary; a late result can be dropped safely even when
an underlying API cannot be interrupted immediately.

GTK source navigation retains its own view-request generation. It answers only whether a current
registry snapshot should be rendered. It does not own authentication, session lifetime, refresh,
failure, or cache authority. Conversely, changing views does not disconnect or cancel a healthy
source refresh merely because its rows are no longer visible.

The registry publishes typed state changes and immutable snapshots. The UI observes those events
and renders fixed translated failure messages. It does not reconstruct lifecycle from spinners,
sidebar row booleans, channel closure, or the currently selected URL.

The closed retained categories are authentication rejection, connection failure, timeout, invalid
response, unsupported authentication, unavailable/permission, and other backend failure, paired
with the connect/refresh/disconnect operation that failed. Cancellation and supersession are state
transitions, not failures. Backend-specific detail may be used transiently to choose a category but
does not enter the registry event or error source chain.

### Media locations are resolved at use

Generic catalogue rows, GTK objects, playback queues, playlist projections, and receiver commands
carry `MediaKey`, metadata, and optional `ViewOrigin`; they do not carry a playable URI. The single
resolution boundary is conceptually:

```text
resolve(MediaKey, MediaUse) -> DirectMedia | ProtectedHttpRequest | LocalFileLease
```

`MediaUse` distinguishes stream, artwork, local playback, and receiver load. Resolution snapshots
the exact ready session epoch, asks the source adapter for its current locator, and rechecks the
epoch before returning. A protected HTTP result carries the current revocable source lease. A
local/removable/external result carries a file-authority lease or an exact revalidated file
locator. An epoch change, disconnect, missing track, ambiguous reconciliation, unavailable mount,
or unsafe file returns a typed unavailable failure; it never falls back to a stale URI or a track
from another source.

Outputs continue to mint receiver-scoped tickets at their own trust boundary. A ticket binds the
resolved session/file lease and is retired on the existing replacement, Stop, end, failure,
ownership-loss, and shutdown paths. Pause and seek may retain a valid ticket only under its current
hard lifetime; they do not renew source ownership.

This changes queue behavior intentionally: an immutable queue preserves order and identity, while
every Play, Next, Previous, repeat, replay, output transfer, and receiver load resolves the current
location. Metadata may remain as a display snapshot. A source refresh or local rename need not
rewrite every queued URI because no queued URI exists.

### Adapter-specific lifecycle

#### Local library and playlists

The local library is one always-registered source. It becomes `Ready` after the database/engine is
available, and each scan or watcher reconciliation is a generation-owned refresh. A database
replacement or application shutdown changes the session epoch. Root-trust authority remains in
the local engine; the registry adapter cannot make an untrusted root playable.

Local playback resolves the exact database `TrackId` at use, obtains the current file path, and
validates it under the current authoritative root before an output or file server receives it.
Playlist navigation queries entries by playlist ID and resolves their linked local track IDs from
the same current snapshot. Fingerprint/path fallback is only a reconciliation operation that
updates `playlist_entries.track_id`; playback never performs fuzzy or path fallback on its own.
An orphan remains visibly unavailable until reconciliation commits a unique match.

A playlist is a `ViewOrigin`, not a source session. Regular and smart playlist queue items carry
the local `SourceId` and local `TrackId`, plus the playlist ID and occurrence for ordering. Deleting
or rebuilding a playlist retires that view's pending navigation while local-source invalidation
still retires its media.

#### Remote libraries, including DAAP

Subsonic, Jellyfin, Plex, and DAAP all occupy ordinary registry entries and expose exact native
track IDs through their adapters. Their endpoint, advertised route, authentication, native media
locator, and protocol metadata remain private to the ready session.

DAAP's adapter additionally implements an asynchronous, idempotent `close`. Explicit disconnect,
replacement, discovery loss, deletion, and shutdown transfer its session into registry-owned
retirement, synchronously revoke media, and elect exactly one logout owner. Concurrent close
callers await the same completion and never send a second logout. A failed or timed-out best-effort
logout still ends in `Dormant`/`Retired`; it cannot resurrect the session. `Drop` performs only
local revocation and memory cleanup and never starts network I/O.

At application close the registry first closes its admission gate, cancels attempts and refreshes,
and revokes every session lease. It then joins all bounded DAAP logout owners and other adapter
teardown before allowing the window to close. A connection completion racing shutdown cannot
escape registry ownership: it is immediately retired and, for DAAP, joined through the same close
path.

#### Internet radio

Radio-Browser is one registered stateless source. Top Clicked, Top Voted, and Near Me are
`ViewOrigin` queries, not three sources; the same station UUID therefore denotes the same
`MediaKey` in every result set. Fetching a feed is a cancellable generation-owned query, including
the geolocation/consent branch for Near Me, and each view retains its last successful snapshot
during refresh. `TrackId` is the station UUID; the current station URL is a locator in the source
adapter and resolves only when playback starts. The adapter retains a locator while at least one
current view snapshot contains that station. It retains contributions per `ViewOrigin`, tagged
with the globally increasing operation generation that produced each accepted snapshot. If live
views disagree about a station locator, resolution selects the contribution with the greatest
accepted generation—therefore the newest initiated successful refresh, not whichever request
completed last. Replacing or deleting a view snapshot removes that view's old contributions and
recomputes the winner from the remaining views; a failed or superseded refresh leaves its prior
accepted contribution intact. Once no current view owns the station, it is unavailable rather than
played from an old URL. The resolved public stream may remain direct because it carries no
Tributary credential.

#### Removable media

Each eligible GIO logical key maps to one deterministic `SourceId`. Mount arrival makes the entry
available and starts a bounded cancellable refresh; relocation of the same key changes the session
epoch and mount locator without changing `SourceId`. Pre-unmount/removal synchronously cancels its
scan, revokes file leases, invalidates the snapshot, and retires playback before the row disappears.

The adapter maps an opaque relative `TrackId` to a path only beneath the current native mount root.
At-use resolution revalidates containment and file type against that current root. The absolute
mount path and old scan URI never enter a queue, so reattachment at the same location cannot revive
stale media. The existing best-available GIO key limitations—including cloned filesystem UUIDs and
path-based fallbacks—remain explicit rather than being disguised as hardware identity.

#### Files opened by the operating system

For an OS delivery containing one or more paths, Tributary tries candidates in order; the first
playable candidate creates an ephemeral one-file source with random `SourceId` and `TrackId`, and
the remaining candidates are not queued. The adapter validates/parses that file and retains the
authority needed to resolve it; the playback queue remains one item. Stop, replacement, end of
stream, failure, or shutdown retires the source and its file lease. An external file is not
silently merged with the local library even when its path currently matches a scanned row. A
future multi-file queue can extend the same ephemeral-source rule explicitly.

## What is already implemented

- `PlaybackSession` captures immutable source/track identity, queue order, duplicate occurrence,
  and playback event generation independently of GTK sorting/filtering.
- Standard remote sources have generation-owned resolvers, random media leases, playback-time
  protected request resolution, and synchronous revocation on replacement/release/shutdown.
- DAAP has generation-owned retained sessions, credential-free references, explicit exactly-once
  logout, a synchronous shutdown gate, and joined retirement.
- Source navigation rejects stale same-key and cross-source publications and preserves the newest
  cache independently of rendering.
- Local database IDs and playlist foreign keys survive authoritative file/directory renames, and
  an incoming full sync refreshes matching queued local locations.
- Removable sources separate a best-available logical key from the mount path and retire scans,
  cache, and playback on relocation/removal.
- Radio rows use station UUIDs, and OS-opened files use a one-item external queue.

These are compatible foundations, not proof that the decision is complete. In particular, current
remote `SourceId` values are still URLs, remote `TrackId` values are still derived UUIDs, the two
remote registries remain siblings, local/playlist/radio/removable/external queues still retain
locations, and lifecycle failures still cross several UI-owned paths.

## Deliberately deferred implementation details

The identity and ownership invariants above are settled. Three representation details are chosen
with the first implementation patch and then frozen by migration fixtures: the checked-in UUID
namespace value, the exact non-lossy relative-path encoding for removable `TrackId`, and the
per-adapter ID length bounds. The chosen bounds must admit every currently supported local path and
provider fixture while bounding hostile catalogue input. The saved-source envelope, legacy-array
reader, and whole-file conflict quarantine above are settled rather than deferred; they do not
introduce a second saved-source database or persist credentials. Exact Rust trait names and
event-channel shapes are internal implementation details so long as one registry enforces this
contract.

## Implementation sequence

1. Introduce `SourceId`, `TrackId`, `MediaKey`, `ViewOrigin`, typed lifecycle events, and the saved
   remote-source schema migration without changing playback behavior.
2. Preserve exact backend-native IDs in catalogue adapters and move the standard remote and DAAP
   resolver contracts behind one registry entry abstraction.
3. Replace URI-bearing queue/UI media fields with `MediaKey`; keep a temporary compatibility
   adapter only at the output boundary.
4. Add the local resolver and make local/playlist navigation, playback, artwork, and receiver loads
   query by ID at use. Remove the random invalid-local-ID fallback.
5. Move Radio-Browser query/locator ownership and removable scan/mount ownership into registry
   adapters, then add the ephemeral external-file adapter.
6. Move connection/refresh cancellation, sanitized failure state, and snapshot publication out of
   GTK callbacks. Remove URL-keyed lifecycle maps and the sibling DAAP registry only after race,
   migration, disconnect, and shutdown tests cover the unified path.

Each step must keep existing credential-isolation, exact-origin, root-authority, receiver-ticket,
and generation-supersession tests green. Compatibility code is removed in the same milestone; two
independent lifecycle systems must not become the permanent architecture.

## Consequences

- Identity survives sorting, filtering, refresh, local relocation, remote reconnect, and explicit
  endpoint rebind without treating a location as the media object.
- Every source kind gets the same cancellation, stale-result, failure, and teardown semantics,
  while adapters retain protocol-specific behavior such as DAAP logout and local root authority.
- Queues and receivers cannot retain a dead local path, stale mount point, old radio stream URL, or
  superseded authenticated session locator.
- The registry becomes an application service with more explicit state and migration code. Source
  adapters and UI event consumers require a staged internal API change.
- Stable IDs do not claim more than their evidence supports. In particular, a removable logical
  key can collide for cloned filesystems, a relative file key does not survive rename, and an
  unsaved remote endpoint that changes URL is a new source until explicitly rebound.

## Rejected alternatives

- **Keep URLs and paths as source/track IDs.** They are mutable locators, can contain sensitive
  material, and make relocation indistinguishable from replacement.
- **Hash every native ID into a global UUID.** Hashing loses the backend value needed for lookup,
  hides namespace mistakes, and cannot migrate a stored hash back to the native ID.
- **Make playlists independent sources.** They are ordered projections of local tracks; duplicating
  source identity would weaken local invalidation and reconciliation.
- **Resolve a playlist fingerprint during playback.** Ambiguous matching belongs in transactional
  reconciliation, not an output path that must choose exactly one track.
- **Perform DAAP logout from `Drop`.** Destructors cannot await, cannot establish exactly-once
  ownership, and race reconnect and process shutdown.
- **Use one undifferentiated generation counter for session and refresh.** Starting a harmless
  refresh would revoke valid playback. Session epochs and operation generations have different
  ownership meanings.

## Completion criteria

This ADR closes only the P3.1 decision and documentation tasks. P3.1 implementation is complete
only when the tracker’s remaining boxes are backed by tests proving stable source migration,
native-ID namespace isolation, exact-generation publication, connect/refresh cancellation,
failure retention, DAAP replacement/disconnect/shutdown, local and playlist ID-at-use resolution,
radio refresh, removable relocation/unplug, and external-file retirement.
