# Source identity and lifecycle ownership

- Status: Accepted in PR #113; stable identity and the unwired lifecycle foundation are implemented,
  adapter consolidation incomplete
- Decision date: 2026-07-17
- Tracker: [P3.1](../task.md#p31-introduce-a-sourcesession-registry)
- Review finding: [Architectural assessment](../../CODE_REVIEW_2026-07-10.md#architectural-assessment)

## Context

Tributary presents local files, playlists, remote libraries, radio stations, removable media, and
files opened by the operating system through one browser and playback surface. When this decision
was recorded, those sources did not share one identity or lifecycle boundary:

- local, playlist, radio, and remote navigation used strings assembled by the UI;
- a remote source's configured URL was also its source key;
- standard authenticated backends and DAAP used separate process-owned registries;
- local, playlist, removable, radio, and external-file queue entries retained a concrete URI;
- remote catalogue identifiers were converted to UUIDv5 values before entering the generic model;
  and
- connection, refresh, cancellation, cached publication, failure, and disconnect behavior was
  split between the GTK window, source-selection callbacks, the local engine, and the registries.

The stable-identity portion has since been implemented. Playback uses an immutable queue addressed
by typed source and exact native track identity instead of a mutable GTK row index. Standard remote
backends retain private native locators behind generation-owned, revocable leases, while DAAP
retains its stateful session and performs explicit logout. Source navigation rejects stale
asynchronous publications. Local renames preserve database track IDs, and removable scans are
generation-owned and cancelled on relocation or removal.

This decision defines how those foundations converge; it does **not** claim that its broader
source/session lifecycle is implemented. PR #120 implements the stable-identity migration, and PR
#121 adds retained local file authority through output consumption; the single lifecycle boundary
remains open. When this decision was recorded, `MediaBackend` was not an integration seam at all.
P3.2 has since completed its bounded
backend-abstraction scope: scanner snapshots construct `LocalBackend`, and all five shipping
backends publish complete track catalogues through one `&dyn MediaBackend` adapter.
Authentication, registry/session lifecycle, refresh/failure ownership, and some browsing paths
still use concrete backend-specific integration, so this decision does not claim the broader
convergence complete. That remaining work stays tracked under
[P3.1](../task.md#p31-introduce-a-sourcesession-registry).

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

The implementation freezes namespace `c931938b-1524-4c8f-b63a-abfa86ce36f1` and the canonical
inputs in the table below. Regression fixtures pin the resulting local, Radio-Browser, and sample
remote UUIDs. `TrackId` accepts at most 256 KiB of adapter-owned identity, while server-controlled
remote IDs and `ViewOrigin` values use a 4 KiB ceiling. `TrackId` debug output reports only its byte
length, so provider-controlled values do not enter diagnostics.

| Source kind | `SourceId` rule | Stability boundary |
|---|---|---|
| Local library | UUIDv5 of `builtin:local` | Constant across launches and library-root changes |
| Saved remote source | Random UUIDv4, or the exact canonical UUIDv5 stored with a migrated/promoted row | Constant across reconnects; UUIDv4 may survive an explicit endpoint rebind, while UUIDv5 is valid only for its canonical endpoint |
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
      "source_id": "f1ee34c2-0b48-5c0d-8eb3-1ccb076c7af9"
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
or rebinds the conflicting record. Version-1 remotes may carry either an RFC UUIDv4 random identity
minted for a manual source or the one exact UUIDv5 derived from that row's canonical `(backend,
base URL)`. Nil, non-RFC UUIDs, every other UUID version, and a UUIDv5 owned by another endpoint,
backend, built-in, or removable source quarantine the complete file. Persisted input therefore
cannot impersonate any deterministic application-owned source, including another remote.
Quarantine is a loader state over the unchanged `servers.json`, not a second persistence store.

The complete version-1 envelope is written to a same-directory temporary file and atomically
replaces the legacy file before any migrated row is published. A write or replacement failure
publishes nothing and preserves the last complete file. A future explicit endpoint rebind may
retain a random UUIDv4 owner. A canonical UUIDv5 owner cannot be carried to a different endpoint in
version 1; rebind must atomically assign a random owner (or introduce a later schema with explicit
rebind provenance) and retire the old owner. Merely discovering a different URL never migrates
identity by name or library similarity.

The existing local `tracks.id` value is already the local backend's native `TrackId` and is kept
byte-for-byte, including a legacy non-UUID string. The random fallback in `db_model_to_track` has
been removed: exact local identity uses the original string, while still-unmigrated UUID APIs see
a frozen deterministic compatibility projection. New local tracks may continue to receive UUIDv4
strings, and playlist foreign keys remain unchanged.

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

The implemented removable spelling is frozen as `unix:<lowercase-hex-native-bytes>` on Unix and
`windows-utf16le:<lowercase-hex-native-code-units>` on Windows after lexical removal of the live
mount root. Empty, outside-root, parent, rooted, and prefixed relative values fail closed. This is
lossless for non-UTF-8 Unix names and unpaired Windows code units, and the same relative spelling
survives a changed mount point.

Tributary treats a provider's native ID as authoritative only inside that source. If a server
itself reuses or changes an ID, no metadata heuristic silently repairs the identity; a refresh
publishes the provider's current mapping, and an explicit source rebind remains the boundary for a
different logical server.

### One registry owns source lifecycle

One application-owned registry is authoritative for every `SourceId`. It stores a deliberate
`SourceSession`/resolver abstraction, not necessarily `Arc<dyn MediaBackend>`. That distinction
allows local, radio, removable, and external-file adapters to participate without forcing every
source lifecycle through `MediaBackend`; P3.2's bounded complete-catalogue seam is already complete.

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
lanes and revoke the current session lease synchronously. Backends cooperate with cancellation at
bounded network/page boundaries only while aborting cannot strand remote ownership. A sessionful
constructor uses the protected construction phase described below: cancellation is observable but
cannot abort the task until a close-capable adapter is under the registry's staged retirement
guard.

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

The local playback adapter resolves the exact database `TrackId` at use, obtains the current file
path, proves it is contained by the most-specific currently configured authoritative root,
acquires retained root/marker/ancestor/file authority, and keeps that authority until the output
or file server has finished consuming it. It rechecks database bindings after acquisition and
rechecks current root configuration before output handoff. Playlist navigation queries entries by
playlist ID and resolves their linked local track IDs from the same current snapshot.
Fingerprint/path fallback is only a reconciliation operation that updates
`playlist_entries.track_id`; playback never performs fuzzy or path fallback on its own. An orphan
remains visibly unavailable until reconciliation commits a unique match. Embedded-art display
still hands its application-owned helper the exact playback-time path rather than retained file
authority; that narrower adapter boundary remains open.

A playlist is a `ViewOrigin`, not a source session. Regular and smart playlist queue items carry
the local `SourceId` and local `TrackId`, plus the playlist ID and occurrence for ordering. Deleting
or rebuilding a playlist retires that view's pending navigation while local-source invalidation
still retires its media.

#### Remote libraries, including DAAP

Subsonic, Jellyfin, Plex, and DAAP all occupy ordinary registry entries and expose exact native
track IDs through their adapters. Their endpoint, advertised route, authentication, native media
locator, and protocol metadata remain private to the ready session.

DAAP's adapter additionally implements an asynchronous `close` that only the registry can
authorize. Explicit disconnect,
replacement, discovery loss, deletion, and shutdown transfer its session into registry-owned
retirement, synchronously revoke media, and elect exactly one logout owner. Repeated disconnect
callers receive clones of one exact retirement waiter and never send a second logout. A failed or
timed-out best-effort logout still ends in `Dormant`/`Retired`; it cannot resurrect the session.
`Drop` performs only local revocation and memory cleanup and never starts network I/O.

DAAP wiring must split its current constructor at the first close-capable boundary. The bounded
`FinishConstruction` phase covers server-info and login only, and returns a logged-in adapter
immediately after parsing the session ID. The registry synchronously installs its mandatory
`StagedConnect` retirement guard before update, database discovery, items, or initial catalogue
work begins. Once the unsafe login request has started, the constructor may observe cancellation
but must not select/drop itself; it finishes and either performs its own ordinary-error cleanup or
returns the adapter for registry retirement. Post-stage catalogue work may be aborted because
dropping the staged guard starts the one tracked close. The current full
`DaapBackend::connect_with_route`/`connect` path must therefore be split during production wiring;
calling that full constructor as the protected authentication closure would stage too late.

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
with the source-wide monotonically increasing operation generation that produced each accepted
snapshot. If live views disagree about a station locator, resolution selects the contribution
with the greatest accepted generation—therefore the newest initiated successful refresh, not
whichever request completed last. Replacing or deleting a view snapshot removes that view's old
contributions and recomputes the winner from the remaining views; a failed or superseded refresh
leaves its prior accepted contribution intact. Once no current view owns the station, it is
unavailable rather than played from an old URL. The resolved public stream may remain direct
because it carries no
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

- `SourceId`, `TrackId`, `MediaKey`, and `ViewOrigin` are typed and bounded. The namespace,
  built-in identities, canonical remote spelling, and removable-path encoding are frozen by
  regression fixtures.
- Saved sources use the strict version-1 envelope. Loading a legacy array derives deterministic
  IDs, collapses canonical duplicates in file order, atomically replaces `servers.json` before
  publication, and publishes nothing when validation or replacement fails. Malformed or unknown
  version-1 data, any identity other than random UUIDv4 or the row's exact canonical remote
  UUIDv5, and endpoint/ID conflicts quarantine the complete unchanged file. Repeated manual Add
  reuses the saved owner; discovered-to-saved promotion persists the row's already-published ID
  before changing its presentation; and saved-plus-env startup authenticates under the stored ID.
  Promotion also retains the live row's ephemeral advertised route and passes it into the
  immediate route-aware authentication/connection attempt; persistence never stores that route.
  Each path therefore keeps one canonical `(backend, endpoint)` owner without transferring live
  ownership between IDs or discarding discovery-only reachability during Add.
- Brand-new manually saved remote rows receive random persisted `SourceId` values. Legacy,
  discovered, environment, and unsaved remote endpoints use deterministic
  backend-plus-canonical-base-URL identity; promoting a discovered/environment row persists that
  existing deterministic ID. The same typed ID is carried through sidebar objects, connect
  generations, standard and DAAP registries, sync events, navigation, disconnect, discovery loss,
  deletion, and shutdown.
- Subsonic, Jellyfin, Plex, and DAAP catalogue rows preserve their exact bounded native song ID,
  item `Id`, `ratingKey`, and decimal `miid`, respectively. Standard opaque playback references
  carry that exact native ID in a reversible, prefixed lowercase-hex segment (so `.` and `..`
  cannot be path-normalized) and their resolvers look it up without a derived UUID compatibility
  projection.
- `PlaybackSession` captures immutable source/track identity, queue order, duplicate occurrence,
  and playback event generation independently of GTK sorting/filtering. Queue identity is a
  `MediaKey`; playlists and radio queries retain a separate `ViewOrigin`, so local invalidation
  retires every local projection while view-specific invalidation cannot retire a sibling view.
- Standard remote sources have generation-owned resolvers, random media leases, playback-time
  protected request resolution, and synchronous revocation on replacement/release/shutdown.
- DAAP has generation-owned retained sessions, credential-free references, explicit exactly-once
  logout, a synchronous shutdown gate, and joined retirement.
- Source navigation rejects stale same-key and cross-source publications and preserves the newest
  cache independently of rendering.
- Local database IDs and playlist foreign keys survive authoritative file/directory renames.
  Architecture rows preserve the exact SQLite ID—including a legacy non-UUID value—and local or
  playlist queues no longer retain a file locator. Every queue load resolves that ID against the
  current row and the most-specific currently configured authoritative root under a five-second
  budget. The resolver retains the root, marker, ancestor chain, and exact regular-file handle,
  rechecks database bindings after acquisition, and hands only typed authority to the output;
  stale generations cannot claim a later load. Local and AirPlay GStreamer, Chromecast, and MPD
  exchange that handle for an opaque app-owned ticket whose bounded explicit-offset stream keeps
  full and Range requests independent even when cloned OS handles share a cursor. Every
  replacement, Stop, error, terminal queue completion, ticket drop, or output teardown retires
  future lookups. Shared Chromecast cleanup retains legacy explicit-file routes while revoking
  credential and retained-authority routes. Playlist queue identity uses the local source plus a
  separate `ViewOrigin`, so generic local retirement also retires a playlist-origin queue without
  losing view navigation.
- Removable sources derive `SourceId` from the best-available logical key and exact `TrackId` from
  the losslessly encoded mount-relative native path. Relocation/removal still retires scans, cache,
  and playback. Radio queues share the built-in Radio-Browser source and reject empty/oversized
  station UUIDs instead of falling back to a stream URL. Each OS-opened file queue receives fresh,
  independent random source and track identities.

These are compatible foundations, not proof that the complete lifecycle decision is implemented.
An initially unwired `SourceLifecycleRegistry` now defines the shared authority boundary without
creating a second production owner. Its entry atomically owns the adopted adapter, production
`MediaLease`, and session epoch. Non-cloneable connect/refresh authority carries exact global
generations and wakeable cancel-before-wait observation. Only atomically admitted spawned tasks and
adapter retirements participate in the persistent shutdown barrier; an unspawned owner is inert,
cannot hold shutdown open, and cannot start work after the gate closes. Dropping the optional task
cancellation capability is also inert. The registry routes explicit cancellation through the
current phase policy, so a protected constructor finishes cooperatively while post-stage work may
be aborted safely. The final external registry handle closes admission, cancels tasks, revokes
leases, and starts fail-closed retirement even if normal shutdown was omitted.

New adapters are wrapped by the framework and synchronously enter either the active session or a
non-cloneable staged retirement guard; no operational handle has close authority. Stale, cancelled,
panicking, rejected, or shutdown results, adopted replacement, explicit disconnect, and shutdown
all use the same exactly-once close path. Repeated disconnect returns one exact reusable waiter;
disconnect/reconnect races dissociate the predecessor close from successor state without losing
its waiter, and a second successor disconnect receives a distinct waiter that the predecessor
cannot complete. A waiter wakes only after locked bookkeeping, typed retirement publication, and
the terminal snapshot transition; replacement and dissociated closes also publish their retired
session epoch so removing a pending-retirement count always advances the observable revision.
Media resolution snapshots the exact adapter/lease/epoch, performs the backend
lookup, rechecks all three, and only then attaches that existing lease to the request.

Source-wide and per-`ViewOrigin` refresh lanes retain their last complete snapshot and accepted
generation. Their sanitized failures carry the exact operation generation and optional session
epoch, so a stale event cannot clear a newer retry. Every provenance publisher owns an opaque keyed
claim; duplicate `Discovery` or removable publishers are reference-counted independently, and a
new claim during close reactivates the logical source while the old retirement remains joined but
cannot mutate a successor. Typed state, snapshot, provenance, cancellation, and failure changes
carry one monotonically increasing registry revision, and only inert, provenance-free retired
entries can be pruned.

The new module is compiled and race-tested but deliberately has no shipping adapter call sites yet.
The standard and DAAP registries therefore remain the production owners, and
connect/refresh/cancellation/failure publication still crosses UI-owned paths. Radio, removable,
and external queue entries have location-independent identity but retain their current locator
until their registry adapters and at-use resolvers are implemented. Local and playlist GTK rows
retain paths for non-playback UI operations; embedded-art display begins with the exact
playback-time resolution but still hands its helper a path rather than retained file authority. A
production row with both saved and discovered provenance still collapses them into one
`manually_added` flag: Delete cannot yet demote it to a still-live discovery publication, and
discovery loss retires live ownership even if the saved row remains. Wiring those production paths
into the new authority, the nonlocal adapters, and the embedded-art authority boundary remain
explicit P3.1 work.

## Deliberately deferred implementation details

The identity and ownership invariants above are settled. The UUID namespace, canonical input
strings, relative-path encoding, and ID bounds are now implemented format state and require an
explicit migration if changed. The saved-source envelope, legacy-array reader, and whole-file
conflict quarantine are also implemented; they do not introduce a second saved-source database or
persist credentials. Exact lifecycle trait names and event-channel shapes remain internal
implementation details so long as one registry ultimately enforces this contract.

## Implementation sequence

1. **Identity complete:** introduce `SourceId`, `TrackId`, `MediaKey`, `ViewOrigin`, and the saved
   remote-source schema migration.
2. **Identity complete, lifecycle open:** preserve exact backend-native IDs in catalogue adapters;
   moving the standard remote and DAAP resolver contracts behind one registry entry remains open.
3. **Identity complete, locator removal partial:** queues carry `MediaKey` and optional
   `ViewOrigin`; remote protected references already resolve at use, while radio/removable/external
   locators remain until their adapters land.
4. **Playback resolution complete, adapter convergence open:** local/playlist playback queries the
   exact ID at use, acquires the current authoritative root and exact file, and retains both
   through output consumption. The random invalid-local-ID fallback is gone. Local embedded art
   and radio/removable/external locators remain on direct helpers until their adapters land.
5. **Identity complete, lifecycle open:** Radio-Browser, removable media, and external files have
   the specified source/track identity; moving their locator and retirement ownership into
   registry adapters remains open.
6. **Lifecycle foundation complete, production wiring open:** the unwired registry now owns the
   state-machine, generation, epoch, provenance, close-task, and persistent shutdown-barrier
   contracts under deterministic race coverage. Move connection/refresh cancellation, sanitized
   failure state, and snapshot publication out of GTK callbacks one adapter family at a time.
   Remove URL-keyed lifecycle maps and the sibling DAAP registry only after standard remotes, DAAP,
   radio, removable, local/playlist, and external-file adapters have entered the unified path.

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
