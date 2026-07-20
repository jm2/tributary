# Subsonic server-native playlist import and pull-sync contract

- Status: Accepted; protocol/authority, persistence/engine, structural UI groundwork, and durable
  full-sidebar ordering implemented; coordinator/browser integration pending
- Decision date: 2026-07-19
- Tracking issue: [#143](https://github.com/jm2/tributary/issues/143)
- Related regular-playlist work: [#140](https://github.com/jm2/tributary/pull/140),
  [#141](https://github.com/jm2/tributary/pull/141), and
  [#142](https://github.com/jm2/tributary/pull/142)
- Server-native foundations: [#144](https://github.com/jm2/tributary/pull/144) and
  [#145](https://github.com/jm2/tributary/pull/145)

This document defines how Tributary may consume playlists owned by a Subsonic or OpenSubsonic
server. It is deliberately separate from
[`source-scoped-playlists.md`](source-scoped-playlists.md): a regular Tributary playlist may already
contain exact tracks from several authenticated sources, while a server-native playlist has a
remote owner, a remote playlist identity, and synchronization state.

The central rule is:

> Server-native playlist integration is an explicit, pull-only snapshot operation. It never turns
> a server URL, credential, session, locator, or playlist name into durable identity, and it never
> grants playback authority merely because an ID appeared in a playlist response.

## Delivery stages

The feature is split into three independently reviewable records:

1. **Implemented — contract, protocol, and exact-session read authority ([#144]).** Define bounded native-playlist identity and
   snapshot types, implement the read-only Subsonic `getPlaylists` and `getPlaylist` endpoints, and
   expose them only through a default-deny current-session registry capability. This stage writes
   no playlist or link state and adds no UI.
2. **Implemented — link persistence and atomic pull synchronization ([#145]).** Add the dedicated
   link schema and the manager operations for detached imports, read-only mirrors, conflict
   detection, successful atomic replacement, missing-server state, unlink, and removal.
3. **In progress — UI, localization, and end-to-end behavior.** Structural header/playlist
   identity, joined durable link presentation, ordinary-action exclusion, commit-only local CRUD,
   atomic smart creation, and the localized recovery-shell plan are implemented in
   [#146](https://github.com/jm2/tributary/pull/146). A follow-up migration and lifecycle-owned
   publisher give scan seeding, CRUD, raw/cascade domain-table writes, and server-link state one
   durable revisioned full-snapshot lane; GTK rejects equal or older delivery. The shell is
   initially hidden and grants no authority. Follow-on slices add the exact-session latest-request
   coordinator, reconnect/shutdown integration, virtualized Import Copy/Keep Synced browser, Sync
   Now, and Retry/Replace/Unlink/Remove recovery with deterministic end-to-end coverage.

No stage may infer permission from a backend label, source key, response shape, or persisted row.
Each consumer uses only the authority implemented by its immediately preceding stage.

## Product modes

### Import Copy

Import Copy reads one complete current server snapshot and creates an ordinary Tributary regular
playlist:

- the imported name and ordered entries are a point-in-time copy;
- exact `(SourceId, TrackId)` identity, order, and duplicate occurrences are preserved;
- after the atomic import commits, no server-playlist link remains;
- the copy is editable with normal regular-playlist operations; and
- later server renames, edits, disappearance, or reconnection do not change it.

Import Copy performs no fuzzy title/artist matching. An entry whose exact track ID is absent from
the current accepted music catalogue is still retained as an unavailable source-scoped occurrence;
it is not silently dropped or replaced with a similarly named local track.

### Keep Synced

Keep Synced creates an opt-in, server-authoritative pull mirror:

- a successful pull replaces the complete local mirror snapshot atomically;
- server order, duplicate occurrences, and exact track IDs are authoritative;
- the linked playlist is read-only in Tributary until the user explicitly unlinks it;
- refresh occurs after a successful source reconnect and through **Sync Now**;
- there is no periodic background polling in the initial implementation; and
- Tributary never calls a create, update, or delete server-playlist endpoint for the mirror.

The mirror is not a bidirectional collaboration surface. Making it editable while linked would
require server mutation authorization, compare-and-swap semantics, positional conflict handling,
owner permissions, and rollback behavior that the common Subsonic API does not provide safely.

## Protocol and feature capability

The read side uses only these authenticated API operations:

- `getPlaylists.view` for bounded playlist summaries; and
- `getPlaylist.view?id=<exact-native-playlist-id>` for one complete ordered entry snapshot.

The API response wrapper, authentication rules, reverse-proxy base-path handling, response-size
limit, request timeout, and sanitized error boundary are the same ones used by the existing
Subsonic client. A successful list must contain its non-null `playlists` object; an explicit empty
object is the only authoritative empty listing, while an absent/null object is malformed. The
detail response must repeat the exact requested playlist ID. A different, missing, empty,
malformed, or oversized ID rejects the complete response. A summary's advertised song count is a
presentation hint; it does not override the actual bounded detail entries and a count mismatch
alone does not reorder, pad, or truncate a snapshot.

`ManagedSourceAdapter` defaults server-native playlists to `Unsupported`. Only the retained,
authenticated Subsonic adapter may advertise `PullSnapshots`; local, Jellyfin, Plex, DAAP,
Radio-Browser, removable-media, external-file, and unknown adapters remain unsupported in this
feature. Advertising the protocol capability means that Tributary may attempt the two read
operations. An HTTP rejection or Subsonic failed envelope remains a closed
backend failure and is never reinterpreted as an empty playlist list. Subsonic-family servers do
not report unsupported endpoints and missing entities consistently enough for the generic client
to infer either state from HTTP status or error code alone. Persistence therefore accepts only
exact absence evidence minted from a successful complete listing; any rejected list/detail retains
the previous local state and grants no capability.

No implemented stage calls create, update, or delete playlist endpoints. Adding those calls
would be a separate product and authority decision, not an incremental extension of
`PullSnapshots`.

## Identity and bounded snapshots

`NativePlaylistId` is a source-native, non-empty UTF-8 string bounded to the same 4,096-byte remote
identity ceiling used by source-scoped playlist tracks. It is preserved byte-for-byte without
trimming, case folding, URL decoding, UUID projection, or metadata fallback. Its debug and error
representations expose only a fixed category and safe size/count context, never its controlled
contents.

A server-playlist summary contains only:

- the exact `NativePlaylistId`;
- bounded optional name and owner presentation hints; and
- an optional advertised track count.

A detailed snapshot contains only:

- the exact `NativePlaylistId`;
- bounded optional name and owner presentation hints; and
- an optional advertised track-count hint; and
- a bounded ordered vector of exact `TrackId` values.

The accepted limits are 16 KiB of UTF-8 for each optional name or owner hint, 10,000 summaries and
an 8 MiB response body for one listing, and 100,000 occurrences and a 64 MiB response body for one
detail snapshot. The body ceiling also bounds the aggregate size of otherwise valid individual
fields. These are safety ceilings, not pagination or truncation targets: crossing one rejects the
complete operation.

Repeated track IDs are valid distinct occurrences and remain repeated in the same order. Empty
playlists are valid. A malformed or oversized member, an excessive list or detail count, or
duplicate native playlist IDs in one listing rejects that complete operation; Tributary never
chooses an arbitrary duplicate or publishes a partial response as complete.

Playlist names and owners are presentation hints, not identity, ownership authority, access
control, or matching evidence. A rename keeps the same playlist only when the exact native ID is
unchanged. A different native ID is a different playlist even if every hint and track is equal.

## Exact-session registry authority

Server-playlist list and detail calls cross `SourceRegistry`, not a UI-owned client or cached
adapter. The registry admits an operation only when all of these remain true:

1. the requested `SourceId` is the currently registered source;
2. its retained adapter is an authenticated Subsonic session;
3. its server-playlist capability is exactly `PullSnapshots`;
4. the captured session epoch and lease are current before adapter work starts; and
5. the same source, adapter instance, epoch, and lease are current after adapter work completes.

Disconnect, replacement, retirement, shutdown, final release, or cancellation rejects a stale
result even when the server call itself succeeded. A successful complete listing owns an opaque
receipt and can derive only an exact presence selection or exact absence evidence. Detail consumes
the presence selection and returns the snapshot with a fresh receipt; predecessor receipts cannot
be reused against a successor or another registry incarnation. Adapter work runs outside the
lifecycle mutex, and revalidation never holds that mutex across network I/O or allows a selector to
re-enter it.

Before a database commit, persistence presents the exact pull or absence receipt back to the same
registry. The registry rechecks its incarnation, source, adapter pointer, epoch, capability, and
active session lease under the lifecycle mutex, then returns an opaque session-only permit. If
invalidation won first, the complete transaction rolls back. If admission won first, replacement,
disconnect, or shutdown waits until commit or rollback drops the permit. The permit carries no
native ID and deliberately does not consult or grant catalogue authority.

An operation result distinguishes adapter-level unsupported authority and current
unavailable/stale authority from a closed backend failure category. Neither variant includes an
endpoint URL, credential, raw server message, response body, native ID, route, or session token.

Playlist endpoint membership is not music-catalogue authority. A track ID present only in a
playlist snapshot remains useful durable identity for a later import or mirror, but it remains
unavailable for display/playback until the normal accepted-catalogue authority independently
contains that exact `(SourceId, TrackId)`. Conversely, an accepted catalogue does not authorize a
server-playlist call.

## Persisted link and atomic manager boundary

Migration 14 adds a dedicated `server_playlist_links` entity rather than overloading regular-
playlist entries. At most one mirror may exist for one exact `(SourceId, NativePlaylistId)`; any
number of detached Import Copies may coexist because they retain no link. One linked row stores only
the minimum non-secret synchronization identity and state:

- the local regular-playlist ID;
- the exact owning `SourceId`;
- the exact `NativePlaylistId`;
- the fixed `pull_read_only_v1` direction and linked/read-only state;
- the last successfully synchronized effective name and a versioned SHA-256 ordered-membership
  digest;
- the last-success UTC-millisecond timestamp;
- orthogonal `clean|conflict` local state and `present|missing` server state; and
- a non-negative monotonic state revision.

The link does not store a server URL, endpoint path, username, credential, token, salt, stream or
artwork locator, source route, adapter object, lease, session epoch, or raw failure. The linked
regular playlist continues to store each media occurrence as canonical `(SourceId, TrackId)` under
the source-scoped storage contract.

The digest format is frozen as `tributary:server-playlist-membership:v1\0`, followed by a
big-endian `u64` occurrence count. Each ordered occurrence contributes a big-endian `u64` source-ID
byte length and exact canonical source-ID bytes, then a one-byte track-presence marker; a present
track contributes its big-endian `u64` byte length and exact native bytes. It excludes occurrence
UUIDs, positions, locators, fingerprints, presentation metadata, and the separately compared
synchronized name. Duplicate occurrences therefore remain significant while cache/reconciliation-
only fields do not create false drift; a malformed legacy unmatched row cannot collide with a
present empty identity.

Every pull or absence check for an existing mirror starts by loading a typed ticket with the exact
link revision. Pull, conflict, and missing updates compare-and-swap that pre-network revision and
increment it; a late completion cannot reload and overwrite newer durable state. Initial detached
import and mirror creation have no prior link revision. The UI-stage operation lane will add
latest-request-start ordering, while the revision remains the crash/restart and persistence
backstop. Downgrade refuses while any link row exists, preventing an older binary from silently
turning a read-only mirror into an editable regular playlist.

Creating an import or mirror and applying a pull are all-or-none database operations. A failed,
cancelled, stale, oversized, or malformed pull changes neither the last complete entry snapshot nor
the last-success metadata. Import Copy never inserts link state. Keep Synced publishes its new
entries, link metadata, digest, timestamp, states, and revision in one commit.

Final lifecycle admission is operation-bound as well as session-bound. The commit permit acquired
from a successful detail pull or complete-list absence carries an opaque seal for that exact result,
and persistence compares the seal before committing. A permit acquired from another pull, another
source, or absence evidence cannot authorize staged data even when both operations still belong to
current sessions.

## Conflict and local-drift policy

The manager refuses ordinary Add, Remove, reorder, rename, delete, smart-rule mutation, and local
reconciliation while a playlist is linked. Persistence compares the effective synchronized name
byte-for-byte and verifies the exact ordered `(SourceId, TrackId)` digest before every pull. This
detects edits from an older binary, direct database access, recovery tooling, or a bug.

If current local state differs from the last successful baseline, a normal pull changes neither
name nor entries nor last-success metadata and marks an explicit conflict. Local conflict and
server absence are orthogonal, so neither state erases the other. Resolution requires one
deliberate action:

- **Replace Local with Server** discards the divergent local mirror only after a fresh current
  snapshot is obtained and commits the complete replacement atomically; or
- **Unlink** retains the current local contents, removes only the link, and turns the playlist into
  an ordinary editable Tributary playlist.

Tributary never merges by name, position, metadata, or longest common subsequence in this feature.
Removing the local copy is a separate explicit destructive action.

## Offline, failure, cancellation, and reconnect behavior

An existing mirror retains its last successful complete snapshot when the source is disconnected,
authentication expires, the server is offline, an endpoint is unsupported, a request times out, a
response is malformed, authority becomes stale, or the operation is cancelled. The UI may show a
localized fixed status and Retry/Sync Now action, but it must not replace the mirror with an empty
list or partially parsed response.

A successful reconnect will schedule one bounded refresh per linked playlist after the accepted source
session is current. A manual Sync Now shares the same deduplicated operation lane. A newer request
or lifecycle retirement cancels older work; only the latest still-current generation may commit.
Record E's structural UI slice now carries authoritative linked/read-only and orthogonal
conflict/missing state into the sidebar without exposing native identity, excludes mirrors from
ordinary mutation affordances, and defines a separate localized status/recovery shell. The shell is
initially hidden and grants no operation authority. Migration 15 and the sidebar publisher now give
scan seeding, ordinary CRUD, raw/cascade domain-table mutations, and server-link state one durable
revisioned full-snapshot lane. Refresh hints coalesce, periodic revision polling recovers lost
hints, and GTK ignores equal or older snapshots. The latest-request lane, exact-session reconnect
scheduling, cancellation, and action wiring remain the next Record E slices; Record D already
rejects stale source receipts and stale persisted revisions. The initial sync version will not
continuously poll the server and will not keep a session alive solely for sync.

## Server rename and deletion

For an imported copy, server rename or deletion is irrelevant because no link remains. For a
mirror, a successful detail response with the same exact native ID may update the mirrored name
along with its entries in the same atomic pull.

A playlist absent from one successful complete current listing can be marked **missing on server**
without deleting or emptying the local snapshot. A failed detail call alone is not deletion
evidence because Subsonic dialects conflate missing, unsupported, and other failures; it retains the
last snapshot as a closed failure until a complete listing or later dialect-aware mapping confirms
the state. The user is offered:

- **Retry**, which keeps the link and asks the current session again;
- **Unlink**, which retains the last snapshot as an editable regular playlist; or
- **Remove Local Copy**, which explicitly deletes the local playlist and link transactionally.

The missing transaction preserves the last synchronized name, membership digest, entries, and
last-success timestamp while independently recomputing whether local drift is clean or conflicting.
Tributary does not find a replacement by name or contents. If the server later exposes the same
exact native ID, Retry may restore the link to current state. A new ID remains a distinct remote
playlist and requires a new import/link decision.

## Privacy, diagnostics, and validation

Server-playlist request diagnostics, architecture DTO debug output, and registry-returned errors
use fixed categories and safe counts. They never interpolate native playlist or track IDs, names
controlled by the server, request URLs, credentials, query strings, response bodies, or raw server
errors. Existing connection/authentication diagnostics retain their established independently
redacted base-URL policy, and transport warnings retain the existing Subsonic security policy,
including the warning for token authentication over HTTP.

Deterministic coverage is required across the staged implementation for:

- default-deny capability and Subsonic-only opt-in;
- authentication parameters and reverse-proxy prefixes for both read endpoints;
- empty lists/playlists, ordered duplicates, count-hint mismatch, ID/detail mismatch, and every
  size/count boundary;
- unavailable, unsupported, disconnected, replaced, retired, cancelled, and stale-session results;
- absence of catalogue membership without accidental playback authority;
- one-time detached import and read-only linked behavior;
- all-or-none pulls, drift conflicts, offline retention, reconnect/manual refresh deduplication,
  server deletion, retry, unlink, and explicit local removal; and
- diagnostic redaction of URLs, credentials, native IDs, server text, and response bodies.

Real-server validation may confirm dialect interoperability later, but it cannot replace these
bounded authority, persistence, and failure-path tests.
