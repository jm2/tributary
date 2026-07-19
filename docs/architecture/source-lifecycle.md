# Source identity and lifecycle ownership

- Status: Accepted in PR #113 and fully implemented through the mounted removable-media adapter;
  P3.1 is complete. Physical removable-hardware, installed Flatpak portal/USB/custom-library, and
  packaged Windows DAAP/Subsonic playback validation remain tracked field work outside this
  decision. P1.5 migration 13 extends the same identity boundary into regular-playlist storage;
  mixed-source rendering/playback remains a follow-up and does not reopen P3.1.
- Decision date: 2026-07-17
- Historical tracker:
  [P3.1](../task-remediation-2026-07.md#p31-introduce-a-sourcesession-registry)
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

The stable-identity and authenticated-remote portions have since been implemented. Playback uses
an immutable queue addressed by typed source and exact native track identity instead of a mutable
GTK row index. For Subsonic, Jellyfin, Plex, and DAAP, GTK rows and queues retain only `SourceId`,
`TrackId`, and the non-secret publishing session epoch; the single `SourceRegistry` owns adapters,
private locators, revocable leases, catalogue/failure state, cancellation, and retirement.
DAAP retains its protocol-specific state and exactly-once logout inside that common lifecycle.
Source navigation rejects stale asynchronous publications. Local renames preserve database track
IDs; local playback and local/playlist embedded-art reads retain exact root/file authority through
consumption. Mounted removable sources now publish pathless registry catalogues and resolve only an
accepted mount-relative identity through retained mount and exact-file authority. Their scans are
generation-owned and cancelled on relocation, pre-unmount, removal, or shutdown.

Regular-playlist storage now follows the same identity rule. Migration 13 assigns every existing
entry to the built-in local source and stores its canonical `(source_id, track_id)` separately from
the nullable local-track foreign-key cache. This foundation can represent another source without
persisting its locator, credentials, lease, or session epoch. The shipping playlist projection is
still deliberately local-only until the next P1.5 slice resolves those identities through the live
registry. The full storage boundary is specified in
[`source-scoped-playlists.md`](../source-scoped-playlists.md).

The implementation has now converged on this boundary. PR #120 implements stable identity, PR #121
adds retained local file authority through output consumption, and PR #122 introduces the generic
lifecycle foundation used by the production cutovers. P3.2 has also completed its bounded
backend-abstraction scope: scanner snapshots construct `LocalBackend`, and all five shipping
backends publish complete track catalogues through one `&dyn MediaBackend` adapter.
Local/playlist embedded-art display consumes a cloned `ResolvedLocalMedia` capability rather than
a path. Radio-Browser publishes three registry-owned views and resolves its public stream locator
only at final playback use. OS-opened files use ephemeral registry-owned adapters, pathless queues,
and a retained exact-file capability through playback and artwork consumption. Mounted removable
media uses the same registry lifecycle, pathless identity, session epoch, and typed file-resolution
branch while retaining its protocol-specific GIO inventory and mount authority. This completes
[P3.1](../task-remediation-2026-07.md#p31-introduce-a-sourcesession-registry); the three remaining
tracker items are physical or installed-environment validation, not an open lifecycle
implementation boundary.

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
strings. Migration 13 makes `(source_id, track_id)` the canonical regular-playlist identity and
retains a separate nullable `local_track_id -> tracks(id) ON DELETE SET NULL` cache so existing
local deletion and reconciliation integrity does not constrain a remote native ID to the local
table.

No complete remote catalogue or playback queue is persisted. A source-scoped regular-playlist row
may retain only the exact `SourceId`, exact native `TrackId`, and safe non-secret match metadata;
it stores no session epoch or media locator. Remote catalogue migration therefore still discards
and refetches process-local catalogues, and never attempts to reverse a one-way UUIDv5 compatibility
projection back into a server ID. If persisted state lacks the exact native identity, the row is
unavailable rather than guessed.

Backend-native `TrackId` values are represented as follows:

| Adapter | Native track identity |
|---|---|
| Local library | Exact SQLite `tracks.id` string |
| Regular-playlist occurrence | The exact `TrackId` of its owning source; currently rendered playlist rows remain local-only pending the mixed-source P1.5 slice |
| Subsonic | Exact song `id` returned by the server |
| Jellyfin | Exact audio item `Id` returned by the server |
| Plex | Exact track `ratingKey`; the media-part key remains a locator |
| DAAP | Canonical decimal `miid` within the DAAP source |
| Radio | Exact Radio-Browser `stationuuid` |
| Removable media | Adapter-owned opaque relative-file key within the logical filesystem; the absolute mount path is never part of the key |
| External file | Random per-open track ID retained by the ephemeral adapter |

Remote IDs are preserved exactly—without trimming, case folding, URL encoding, or global UUID
hashing—after applying a documented ingestion size bound. An empty or over-limit ID rejects that
catalogue item. The removable adapter uses a lossless encoded relative path as its native key
because no platform-independent file ID is available; rename stability on a device is not
promised. Its current source generation maps that key to a validated locator beneath the live
retained mount authority. A stronger device-native file ID can replace that representation later
without exposing an absolute path to generic state.

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

`MediaUse` distinguishes stream, artwork, local playback, and receiver load. Production stream
resolution exposes the closed `ResolvedSourceStream::Http` or `ResolvedSourceStream::File` result
instead of returning a separable locator and lease. Resolution snapshots
the exact ready session epoch, asks the source adapter for its current locator, and rechecks the
epoch before returning. A protected HTTP result carries the current revocable source lease. A
local, removable, or external result carries the retained exact-file capability with its current
authority lease. An epoch
change, disconnect, missing track, ambiguous reconciliation, unavailable mount, or unsafe file
returns a typed unavailable failure; it never falls back to a stale URI or a track from another
source.

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
rechecks current root configuration before output handoff. The current playlist navigation path
queries entries by playlist ID and resolves their nullable `local_track_id` cache from the same
current snapshot. Stored non-local identities remain outside that projection until live-registry
resolution is implemented.

Fingerprint/path fallback is only a local reconciliation operation. It is restricted to the exact
built-in local `source_id` and updates both canonical `track_id` and `local_track_id` only after a
unique match commits; playback never performs fuzzy or path fallback on its own. An unmatched
occurrence remains unavailable until reconciliation commits. Embedded-art display
clones the exact accepted `ResolvedLocalMedia`, revalidates its physical authority when cloning the
file, and owns that handle through generation-checked parsing. No local/playlist artwork helper
receives or reopens the playback-time path.

A playlist is a `ViewOrigin`, not a source session. Each durable regular-playlist occurrence now
names its actual media owner with `(source_id, track_id)` while keeping the playlist ID, entry ID,
and position as view/occurrence state. The current regular- and smart-playlist queue path still
publishes only local rows; mixed-source GTK rows, current-epoch adoption, playback, and source-
retirement behavior are the next P1.5 record. Deleting or rebuilding a playlist retires that view's
pending navigation without pretending that the view owns any contributing source session.

#### Remote libraries, including DAAP

Subsonic, Jellyfin, Plex, and DAAP all occupy ordinary registry entries and expose exact native
track IDs through their adapters. Their endpoint, advertised route, authentication, native media
locator, and protocol metadata remain private to the ready session.

Interactive Jellyfin `AuthenticateByName` can mint a server-side session token before an adapter
exists. Its bounded constructor therefore uses `FinishConstruction`, synchronously stages the
token-bearing adapter before ping, library discovery, or catalogue work, and gives the registry's
close capability the only authority to POST `Sessions/Logout`. A pre-existing Jellyfin API key is
not a session minted by Tributary, so its constructor is abortable and disconnect never revokes
that durable credential. Plex's legacy token is likewise treated as a durable credential: the
available revocation mechanisms are broader than one adapter session, so retirement revokes local
media/adapter authority without attempting an account- or device-wide server-side revocation.

There is one earlier Jellyfin cleanup boundary before registry staging is possible. Once
`AuthenticateByName` returns a token that can be represented exactly as a sensitive
`X-Emby-Authorization` value, failure to construct the final routed authenticated client triggers
one bounded best-effort `Sessions/Logout` through the exact pre-authentication transport; the
original redacted construction failure remains authoritative. A hostile server can instead return
a token containing HTTP control bytes. Such a token cannot safely form the exact logout header, and
sending it raw or transformed would permit header injection or target a different session. That
narrow case therefore fails closed without echoing the token and intentionally cannot attempt
server-side logout.

DAAP's adapter additionally implements an asynchronous `close` that only the registry can
authorize. Explicit disconnect,
replacement, discovery loss, deletion, and shutdown transfer its session into registry-owned
retirement, synchronously revoke media, and elect exactly one logout owner. Repeated disconnect
callers receive clones of one exact retirement waiter and never send a second logout. A failed or
timed-out best-effort logout still ends in `Dormant`/`Retired`; it cannot resurrect the session.
`Drop` performs only local revocation and memory cleanup and never starts network I/O.

DAAP now implements that close-capable boundary explicitly. `DaapClient::login_with_route` covers
server-info and login only and returns immediately after parsing `mlid`; `DaapBackend` then enters
the registry's bounded `FinishConstruction` phase. The registry synchronously installs its
mandatory staged-retirement guard before `load_catalogue` begins update, database discovery,
items, or initial catalogue publication. Once login has started, cancellation is observed without
dropping an unowned server session; login either fails before session ownership exists or returns
the close-capable adapter for registry retirement. Post-stage catalogue work may be aborted because
dropping the staged guard starts the one tracked close. A malformed or expired post-login update,
database, or items response therefore publishes no catalogue or media and reaches that same exact
logout owner.

At application close the registry first closes its admission gate, cancels attempts and refreshes,
and revokes every session lease. It then joins all bounded DAAP and owned Jellyfin logout work plus
other adapter teardown before allowing the window to close. A connection completion racing
shutdown cannot escape registry ownership: it is immediately retired and joined through the same
close path when it owns server-side session state. The menu and Ctrl+Q application actions request
that window close rather than calling application quit directly, so the same `close-request`
barrier runs; direct application quit is reserved for the no-window case.

#### Internet radio

Radio-Browser is one registered stateless source. Top Clicked, Top Voted, and Near Me are
`ViewOrigin` queries, not three sources; the same station UUID therefore denotes the same
`MediaKey` in every result set. Fetching a feed is a cancellable generation-owned query, including
the geolocation/consent branch for Near Me, and each view retains its last successful snapshot
during refresh. While first-use consent is open, an exact generation-owned prerequisite marker
distinguishes that deliberate pre-construction interval from source loss; stale/superseded dialog
requests cannot keep that exception current. Construction failure or later source loss returns a
selected radio lane to Local and restores the user's normal music-column and browser-visibility
preferences before rendering Local rows. `TrackId` is the station UUID; the current station URL is
a locator in the source adapter and resolves only when playback starts. The adapter retains a
locator while at least one
current view snapshot contains that station. It retains contributions per `ViewOrigin`, tagged
with the source-wide monotonically increasing operation generation that produced each accepted
snapshot. If live views disagree about a station locator, resolution selects the contribution
with the greatest accepted generation—therefore the newest initiated successful refresh, not
whichever request completed last. Replacing or deleting a view snapshot removes that view's old
contributions and recomputes the winner from the remaining views; a failed or superseded refresh
leaves its prior accepted contribution intact. Once no current view owns the station, it is
unavailable rather than played from an old URL. A resolved public request is still provisional:
immediately before output load it rechecks the exact greatest contributing generation through weak
registry authority and its source and accepted-view leases. View replacement/removal, a newer
overlapping view, source replacement/disconnect, or the last registry handle dropping therefore
revokes a request that was already resolved. The request may then yield its public URL directly
because it carries no Tributary credential.

#### Removable media

Each eligible GIO logical key maps to one deterministic `SourceId`. The GIO key remains
main-thread inventory/controller metadata, while the native mount path passes only into private
adapter construction and retained filesystem authority; neither enters generic source state.
Stable `SourceId` text is the UI, cache, navigation, and lifecycle key. Mount arrival claims
provenance and automatically starts a bounded cancellable registry connection rather than waiting
for row selection to launch a second scanner. Relocation
of the same logical key disconnects the predecessor before reconnecting the fresh inventory under
a new session epoch without changing `SourceId`.

Construction walks the exact retained native mount on Tokio's blocking pool with no link following,
same-filesystem containment, deterministic ordering, cooperative cancellation, and bounded tag
metadata. Each candidate is parsed through an exact handle acquired beneath that retained mount.
The accepted catalogue contains only `SourceId`, losslessly encoded mount-relative `TrackId`,
metadata, and its publishing epoch; neither the absolute mount path nor a `file://` locator enters a
generic row, cache, queue, or receiver request.

At-use resolution first requires exact membership in that accepted catalogue, decodes only that
bounded native relative identity, and then revalidates the retained mount, ancestor namespace,
containment, regular-file type, and exact file before returning `ResolvedSourceStream::File`.
Replacement at the pathname cannot retarget the retained read. Unix root acquisition rejects a
symlink even when the spelling ends in a slash or `/.`; Windows traversal pins the namespace
against raced junction replacement and follows a final reparse only when it is verified as a
volume mount. The final exact file retains sharing compatible with unmount while the short-lived
namespace guards prevent a traversal escape.

Pre-unmount synchronously disconnects the adapter, cancels its scan, revokes file leases, clears
its accepted snapshot, and retires playback while retaining the logical provenance claim in case
the unmount fails. Confirmed removal performs the same idempotent retirement before removing UI
state and releasing the claim. A later appearance therefore constructs a fresh adapter and epoch;
reattachment at the same path cannot revive stale authority.

Because pathless removable rows deliberately expose no mutation path, the Properties action is
omitted until a typed retained mutation authority exists. That safe UI limitation does not weaken
playback or artwork, which both consume clones of the accepted retained file capability. The
best-available GIO-key limitations—including cloned filesystem UUIDs and path-based fallbacks—remain
explicit rather than being disguised as hardware identity.

#### Files opened by the operating system

For an OS delivery containing one or more paths, one blocking worker opens and parses candidates
sequentially in delivery order. Invalid or unsupported candidates are skipped and processing stops
after the first file that parses as audio and passes the metadata bounds; the remaining candidates
are not queued. Before minting identity or publishing an adapter, the worker must still own the
delivery's exact admission generation under the same gate that serializes publication with
shutdown. A newer OS delivery, any explicit
Play/Pause/Next/Previous/scrub action, Stop, a real output change, or shutdown advances that
generation, so superseded parsing cannot become visible. Same-output reselection is deliberately
inert. Logs report only a count and fixed outcome; an OS-delivered path or derived direct URI never
enters diagnostics. A native non-UTF-8 leaf name becomes bounded lossy Unicode only as a parser and
presentation hint; it never replaces the already-open handle as authority.

Only after the exact already-open file parses successfully does Tributary mint random `SourceId`
and `TrackId` values and atomically adopt an ephemeral hidden adapter. That adapter owns the
original open regular-file handle and a revocable `MediaLease`; `try_clone_file` checks the lease
both before and after cloning the handle. Cursor-based tag and artwork consumers serialize across
their complete clone-and-parse operation, while output proxies keep using position-independent
reads. The one-item queue carries only those random IDs and the exact session epoch—no path or
URI—and generic resolution returns the typed
`ResolvedSourceStream::File` capability. HTTP-backed sources continue through the sibling
`ResolvedSourceStream::Http` branch. Embedded-art parsing receives a clone of the retained file
only after the selected output has accepted the load, preventing rejected or stale work from
gaining an artwork read.

The hidden adapter is retired explicitly and idempotently on replacement by another external or
ordinary queue, Stop, unrepeated end-of-stream, playback or load failure, real output change, and
shutdown. A stale worker that loses admission after adoption retires its own exact session before
discarding it. Registry shutdown closes admission under the same publication gate, so it cannot
leave a late hidden source behind. A lifecycle baseline clears hidden entries only when they own a
real published UI projection or pending sidebar state; adopting an intentionally hidden external
source therefore does not invalidate its own playback. An external file is not silently merged
with the local library even when its path currently matches a scanned row. A future multi-file
queue can extend the same ephemeral-source rule explicitly.

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
  generations, `SourceRegistry`, lifecycle baselines, navigation, disconnect, discovery
  loss, deletion, and shutdown.
- Subsonic, Jellyfin, Plex, and DAAP catalogue rows preserve their exact bounded native song ID,
  item `Id`, `ratingKey`, and decimal `miid`, respectively. Their GTK rows and playback queues
  carry only pathless `SourceId`, exact `TrackId`, and the non-secret session epoch that published
  the catalogue. Resolution uses the exact native ID without a derived UUID compatibility
  projection or a generic authenticated/lease-bearing URI.
- Regular-playlist migration 13 brings durable membership under the same identity namespace.
  Existing entry IDs, order, duplicate occurrences, local links, and reconciliation evidence are
  preserved while each row gains the exact built-in local `source_id`; canonical `track_id` is
  separated from a nullable `local_track_id` foreign-key cache. New non-local storage accepts only
  typed source/native identity plus optional non-secret normalized fingerprints and rejects file-
  path evidence. Those fingerprints are neither display fields, identity, nor matching authority;
  migration 13 adds no source-label snapshot. It persists no source epoch, locator, route, lease, or
  credential. Mixed-source publication and playback remain deliberately unwired in this storage
  slice.
- `PlaybackSession` captures immutable source/track identity, queue order, duplicate occurrence,
  and playback event generation independently of GTK sorting/filtering. Queue identity is a
  `MediaKey`; playlists and radio queries retain a separate `ViewOrigin`, so local invalidation
  retires every local projection while view-specific invalidation cannot retire a sibling view.
- Authenticated remotes share one `SourceRegistry` with generation-owned adapters, random
  media leases, exact session epochs, immutable catalogue/failure snapshots, playback-time
  protected request resolution, and synchronous revocation on replacement, route loss, release,
  or shutdown. DAAP and interactively authenticated Jellyfin add explicit exactly-once logout
  through the same tracked/joined retirement path; pre-existing Jellyfin API keys and Plex legacy
  tokens are retained as non-owned durable credentials rather than broadly revoked.
- The same `SourceRegistry` installs one built-in stateless Radio-Browser adapter. Top Clicked, Top
  Voted, and Near Me are exact independently cancellable view lanes. Accepted snapshots expose
  pathless `Track` values while their private payload retains validated station-ID-to-public-URL
  contributions and a revocable view lease. A failed successor preserves the prior accepted view;
  an accepted empty result authoritatively replaces it. Overlapping station IDs resolve from the
  greatest accepted source-wide generation and are rechecked again at final consumption through
  weak authority, so neither an obsolete view nor a pending request can retain or retarget a
  source. Near Me asks for translated location consent first, tolerates partial successful tiers,
  deduplicates in tier order, and then applies one stable global distance ordering. Automatic
  source loss restores Local's configured music-column and browser presentation.
- Source navigation rejects stale same-key and cross-source publications and preserves the newest
  cache independently of rendering.
- Local database IDs and the playlist entry's canonical/local-cache pair survive authoritative
  file/directory renames.
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
  credential and retained-authority routes. The currently shipping playlist queue identity uses
  the local source plus a separate `ViewOrigin`, so generic local retirement also retires a
  playlist-origin queue without losing view navigation. A later P1.5 slice will select the source
  per stored occurrence and retain that same separation between media owner and playlist view.
- Removable sources derive `SourceId` from the best-available logical key and exact `TrackId` from
  the losslessly encoded mount-relative native path. Their registry-owned adapter publishes a
  pathless, epoch-bound accepted catalogue; retained mounted-root authority, an exact membership
  gate, and no-follow same-filesystem traversal protect scan and at-use file resolution. Relocation,
  pre-unmount, removal, and shutdown disconnect before invalidating cache, navigation, playback,
  and provenance. Radio queues share the built-in Radio-Browser source and reject empty/oversized
  station UUIDs instead of falling back to a stream URL.
- Each accepted OS-opened file owns a hidden registry adapter with fresh independent random source
  and track identities plus the exact adopted epoch. Ordered first-accepted-audio admission runs on
  a blocking worker and rechecks its exact generation under the shutdown/publication gate before
  identity is minted or the retained already-open file is adopted. Its one-item queue is pathless;
  stream resolution returns only a lease-checked file capability, embedded art clones that
  capability after output acceptance, and every terminal or superseding boundary retires the
  ephemeral adapter idempotently. Count-only OS-open diagnostics expose no path.

The initially unwired `SourceLifecycleRegistry` now backs the shipping `SourceRegistry`. For
Subsonic, Jellyfin, Plex, DAAP, the built-in Radio-Browser source, mounted removable sources, and
ephemeral external-file sessions, each entry atomically owns the adopted adapter, production
`MediaLease`, session epoch, operation generations, immutable accepted snapshots where applicable,
keyed provenance, and sanitized failure state. Non-cloneable operation authority carries exact
global generations and wakeable cancel-before-wait observation. Only atomically admitted tasks and
adapter retirements participate in the persistent shutdown barrier; an unspawned owner is inert,
cannot hold shutdown
open, and cannot start work after the gate closes. The registry routes cancellation through the
current phase policy, so DAAP's protected login finishes cooperatively while post-stage catalogue
work may be aborted safely. The final external registry handle closes admission, cancels tasks,
revokes leases, and starts fail-closed retirement even if normal shutdown was omitted.

Adapters are wrapped by the framework and synchronously enter either the active session or a
non-cloneable staged-retirement guard; no operational handle has close authority. Stale,
cancelled, panicking, rejected, or shutdown results, adopted replacement, explicit disconnect,
discovery route loss, and shutdown all use the same exactly-once close path. Repeated disconnect
returns one exact reusable composite waiter. Each spawned connection generation owns a
`ConnectSettlement` participant through construction, including superseded generations. If it
constructs a late adapter that must be rejected, participant ownership transfers into that exact
close job before the constructor participant drops; the waiter can never observe a false zero-count
gap. A disconnect joins its adopted-adapter retirement, the latest dissociated predecessor close,
and every still-active per-generation settlement in deterministic generation order. It completes
only after all constructors and late closes settle, and reports a late rejected-adapter close
failure as a sanitized disconnect failure. Disconnect/reconnect races dissociate the predecessor
close from successor state without losing its waiter, while a second successor disconnect receives
a distinct waiter that the predecessor cannot complete. Final provenance release records
`Retired` even when the existing disconnect is settlement-only, then lifecycle-owned maintenance
awaits that waiter and prunes the inert entry without a duplicate cancel or close. Media resolution
rejects a mismatched expected epoch before invoking the adapter, snapshots the exact
adapter/lease/epoch, performs the backend lookup, rechecks all three, and only then attaches that
existing lease to the request.

One atomic `LifecycleBaseline` and monotonic invalidation watch are now the production GTK input.
The reducer renders state, catalogue, failure, provenance, visibility, cancellation, and retirement
without reconstructing authority from spinners, URLs, row existence, or channel closure. A
same-epoch catalogue refresh preserves the current queue and navigation; a new epoch invalidates
stale media before publishing its successor. Catalogue acceptance clears its exact pending guard
before any connected-row rebind or programmatic selection. If the exact accepted row was already
selected but the guarded rebind could not activate it, the reducer invalidates selection and
reselects that same index after catalogue/cache state is authoritative. Only the exact accepted
generation can plan this reactivation; stale or superseded catalogues remain cache-only or inactive.
GTK helpers clone `RefCell`-backed active keys and release all navigation borrows before changing
`GtkSingleSelection`, whose signals may synchronously re-enter those cells. Generation-correlated
failure or cancellation clears only its exact pending intent, including a result observed only
through a resnapshot. Hidden or absent lifecycle state authoritatively clears pending state, cache,
playback, navigation, sidebar row, and empty category header only for sources that actually own one
of those published UI projections. Intentionally hidden external-file adapters therefore remain
playback-owned rather than being erased by the next baseline. Lifecycle-owned pruning waits for the
current retirement to finish without issuing another mutating disconnect.

Saved, Environment, and Discovery publishers own independent opaque keyed claims. Duplicate
publishers are reference-counted, and a new claim during close reactivates the logical source while
the old retirement remains joined but cannot mutate a successor. Removing Saved demotes a row that
still has Discovery instead of deleting it. Removing Discovery clears the advertised route and
revokes the active adapter or pending constructor that may have captured that route, even when
Saved or Environment keeps the logical row visible. Route withdrawal therefore retires live media,
cache, and active projection without incorrectly deleting a still-claimed row. The reducer derives
the presentation-only `manually_added` value from the live Saved claim rather than treating that row
flag as provenance authority.

Comprehensive deterministic lifecycle, registry-wrapper, reducer, playback-boundary, provenance,
shutdown, and actual-wire DAAP regressions cover protected login cancellation, supersession,
malformed post-login catalogue responses, exact logout, invalid Jellyfin token containment,
stale-epoch stream/artwork rejection, accepted-catalogue reactivation, `RefCell` signal re-entry,
failure correlation, route-withdrawal demotion/visibility, composite disconnect settlement, and
retirement/pruning races. External-file regressions additionally cover delivery-order and exact
admission state, shutdown/publication races, pathless identity and epoch checks, retained-handle
behavior after path replacement, lease revocation, hidden-baseline ownership, and exactly-once
retirement. Independent review covers the complete intent/terminal wiring and post-accept artwork
handoff. Removable regressions additionally cover lossless native identity, unsafe-component
rejection, no-follow and same-filesystem mount authority, exact accepted-membership enforcement,
reconnect epoch/lease isolation, queued-scan cancellation, shutdown joining, lifecycle/UI
invalidation ownership, and pathless playback/artwork. The focused lifecycle module passes all 53
tests.

The authenticated-remote, Radio-Browser, external-file, and mounted-removable cutovers complete the
P3.1 adapter boundary. Local and playlist GTK rows may retain paths for non-playback UI operations,
but those paths no longer cross the embedded-art boundary. After the exact local ID has resolved,
its configuration and generation are still current, and the selected output accepts the load,
playback gives the art worker a clone of the same `ResolvedLocalMedia` authority. The worker
revalidates the root marker, ancestor chain, and exact file while cloning the handle and owns that
capability through parsing. Replacement at the pathname cannot retarget the read, and authority
drift fails closed. External-file playback and artwork similarly retain and clone the already-open
lease-protected capability rather than reopening a pathname. Removable playback and artwork use the
same typed retained-file branch after exact epoch and catalogue-membership checks.

The retained reader rewinds before every attempt and when it finishes because cloned operating-
system handles can share a cursor. Lofty receives only the safe extension hint, uses a content
probe only for an unknown suffix, and skips unrelated property reads. Its explicit MP4 reread and
raw `covr` fallback operate on the same handle; the raw fallback caps the complete file image at
256 MiB, caps returned artwork at 32 MiB, and checks every atom offset, size conversion, and
addition. The ordinary Lofty path applies the same 32 MiB artwork cap. A separate art generation
check rejects a result superseded while parsing. The legacy direct-file artwork helper is no longer
on the removable playback path.

## Deliberately deferred implementation details

The identity and ownership invariants above are settled. The UUID namespace, canonical input
strings, relative-path encoding, and ID bounds are now implemented format state and require an
explicit migration if changed. The saved-source envelope, legacy-array reader, and whole-file
conflict quarantine are also implemented; they do not introduce a second saved-source database or
persist credentials. Exact lifecycle trait names and event-channel shapes remain internal
implementation details so long as one registry enforces this contract. A typed retained mutation
authority for pathless removable Properties editing is deliberately deferred; the UI omits that
action instead of reconstructing a path or weakening playback authority. Source-scoped regular-
playlist storage is specified separately, while mixed-source row resolution, playback, lifecycle
refresh, and Subsonic-native playlist synchronization remain explicitly deferred P1.5 work.

## Implementation sequence

1. **Identity complete:** introduce `SourceId`, `TrackId`, `MediaKey`, `ViewOrigin`, and the saved
   remote-source schema migration.
2. **Authenticated-remote lifecycle complete:** exact backend-native IDs and Subsonic, Jellyfin,
   Plex, and DAAP adapter/resolver contracts now live behind `SourceRegistry` entries.
3. **Identity and locator removal complete:** queues carry `MediaKey`, optional `ViewOrigin`, and
   the publishing epoch where required. Authenticated remotes, radio, external files, and removable
   media are pathless; locators and retained file authority resolve only at use.
4. **Playback resolution and adapter convergence complete:** local/playlist playback queries the
   exact ID at use, acquires the current authoritative root and exact file, and retains both
   through output consumption. The random invalid-local-ID fallback is gone. Radio resolves its
   validated public locator from the exact current accepted view; external files resolve a retained
   already-open capability; removable media resolves an accepted relative identity beneath its
   retained mounted-root authority.
5. **Radio-Browser, external-file, and removable lifecycle complete:** Radio-Browser is one
   stateless registered source with three exact view lanes, private locator contributions, and
   final-consumption authority. Each external file is one ephemeral hidden registered source with
   generation-owned admission, retained file authority, pathless identity, and explicit retirement.
   Each mounted removable source is a provenance-claimed registered adapter with a cancellable
   blocking scan, epoch-bound pathless catalogue, and disconnect-before-invalidation lifecycle.
6. **Production cutovers complete:** the lifecycle registry owns the state machine, generation,
   epoch, provenance, close task, persistent shutdown barrier, and coherent baseline/watch contracts
   under deterministic race coverage. Subsonic, Jellyfin, Plex, DAAP, Radio-Browser, external-file,
   and mounted-removable connection/publication, sanitized failure state, media resolution,
   disconnect, and shutdown use that production path. The URL-keyed standard owner, sibling DAAP
   registry, and removable direct-URI scanner/playback path are removed from production use.
7. **Local embedded-art authority complete:** local and playlist playback clone the accepted
   `ResolvedLocalMedia` into a generation-checked art worker. It revalidates and retains the exact
   file capability through bounded, cursor-safe parsing without reopening a pathname. External art
   and removable art follow the same post-accept retained-capability rule.

The authenticated-remote cutover's locked debug and release suites each passed 20 library, 865
application, and 10 repository-metadata tests (895 total), with locked all-target/all-feature
compile, strict warning-free Clippy, formatting, and diff checks green.

PR #125 validation for the retained embedded-art slice passes all 9 focused album-art
tests, the locked all-target/all-feature check, strict Clippy in debug and release, formatting, and
the whitespace check. Locked debug and release suites each pass 20 library, 872 application, and
10 repository-metadata tests (902 total).

The Radio-Browser cutover passes the locked all-target/all-feature check, strict Clippy in debug and
release, formatting, and whitespace checks. Complete locked debug and release suites each pass 20
library, 895 application, and 10 repository-metadata tests (925 total). Focused lifecycle,
source-registry, media, radio-client/adapter, reducer, consent, queue, and playback tests cover
cancellation, empty/failure distinction, cross-view winner ordering, final-use revocation,
last-registry-drop, partial Near Me tiers, deduplication/distance ordering, pathless capture,
pre-publication source loss, exact failure ownership, the generation-owned consent prerequisite,
and complete restoration of Local presentation after automatic fallback.

The external-file adapter cutover passes the locked all-target/all-feature check, strict Clippy in
debug and release, formatting, and whitespace checks. Complete locked debug and release suites each
pass 940 tests. An independent integrated review is clean after its findings were resolved. Focused
coverage proves delivery-order and exact-generation state, shutdown/adoption serialization, random
pathless identity, epoch isolation, retained-handle path-replacement resistance, lease checks,
serialized cursor-based consumers, hidden-baseline behavior, and explicit idempotent retirement;
integrated review covers sequential first-accepted candidate handling, the same-output boundary,
and post-accept artwork wiring.

The mounted-removable cutover passes the locked all-target/all-feature check, strict debug Clippy,
formatting, and whitespace checks. The complete locked debug suite passes 20 library, 926
application, and 10 repository-metadata tests (956 total). Focused identity, mounted-authority,
adapter, registry, navigation, reducer, context-menu, and playback regressions cover pathless
catalogues, exact membership, retained-handle resolution, cancellation, reconnect epoch isolation,
shutdown settlement, relocation/removal ownership, retained failure replay on later selection, and
safe omission of path-based mutation. The
three remaining tracker tasks require physical removable hardware, an installed interactive
Flatpak environment, or packaged Windows DAAP/Subsonic servers and are not unimplemented P3.1
lifecycle work.

Each step must keep existing credential-isolation, exact-origin, root-authority, receiver-ticket,
and generation-supersession tests green. Compatibility code is removed in the same milestone; two
independent lifecycle systems must not become the permanent architecture.

## Consequences

- Identity survives sorting, filtering, refresh, local relocation, remote reconnect, and explicit
  endpoint rebind without treating a location as the media object.
- Every source kind gets the same cancellation, stale-result, failure, and teardown semantics,
  while adapters retain protocol-specific behavior such as DAAP logout and local root authority.
- Local/playlist queues, receivers, and embedded-art parsing cannot retain or reopen a dead library
  path, and path replacement cannot retarget a retained read. Authenticated-remote and radio queues
  likewise retain identity and epoch rather than a credential-bearing or public locator. External
  queues retain only random identity, epoch, and a registry-owned open-file capability. Removable
  queues retain only stable source identity, lossless relative track identity, epoch, and a
  registry-owned retained mounted-file capability.
- The registry becomes an application service with more explicit state and migration code. Source
  adapters and UI event consumers require a staged internal API change.
- Stable IDs do not claim more than their evidence supports. In particular, a removable logical
  key can collide for cloned filesystems, a relative file key does not survive rename, and an
  unsaved remote endpoint that changes URL is a new source until explicitly rebound.
- Regular-playlist persistence can retain a source-scoped occurrence while its source is offline,
  but storage alone does not make it playable. Only the same live source and exact native track can
  restore authority; metadata and endpoint similarity are never substitutes.

## Rejected alternatives

- **Keep URLs and paths as source/track IDs.** They are mutable locators, can contain sensitive
  material, and make relocation indistinguishable from replacement.
- **Hash every native ID into a global UUID.** Hashing loses the backend value needed for lookup,
  hides namespace mistakes, and cannot migrate a stored hash back to the native ID.
- **Make playlists independent sources.** They are ordered projections over media owned by their
  contributing sources; duplicating source identity would weaken lifecycle invalidation and
  reconciliation.
- **Resolve a playlist fingerprint during playback.** Ambiguous matching belongs in transactional
  reconciliation, not an output path that must choose exactly one track.
- **Perform DAAP logout from `Drop`.** Destructors cannot await, cannot establish exactly-once
  ownership, and race reconnect and process shutdown.
- **Use one undifferentiated generation counter for session and refresh.** Starting a harmless
  refresh would revoke valid playback. Session epochs and operation generations have different
  ownership meanings.

## Completion criteria

This ADR closes the P3.1 decision, implementation, and documentation tasks. Automated regressions
prove stable source migration, native-ID namespace isolation, exact-generation publication,
connect/refresh cancellation, failure retention, DAAP replacement/disconnect/shutdown, local and
playlist ID-at-use resolution, retained local/playlist embedded-art parsing, radio refresh,
removable relocation/retirement semantics, and external-file retirement. Mounted removable media
now has registry-owned scanning, pathless catalogue/queue identity, retained at-use file authority,
epoch/lease revocation, and disconnect-before-invalidation UI wiring. P3.1 is complete. Physical
unplug/relocation, installed Flatpak portal/USB/custom-library behavior, and packaged Windows
DAAP/Subsonic playback remain separate manual validation tasks; they do not keep this ADR open.
