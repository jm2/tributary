# Subsonic server-native playlist import and pull-sync contract

- Status: Accepted for staged implementation
- Decision date: 2026-07-19
- Tracking issue: [#143](https://github.com/jm2/tributary/issues/143)
- Related regular-playlist work: [#140](https://github.com/jm2/tributary/pull/140),
  [#141](https://github.com/jm2/tributary/pull/141), and
  [#142](https://github.com/jm2/tributary/pull/142)

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

1. **Contract, protocol, and exact-session authority.** Define bounded native-playlist identity and
   snapshot types, implement the read-only Subsonic `getPlaylists` and `getPlaylist` endpoints, and
   expose them only through a default-deny current-session registry capability. This stage writes
   no playlist or link state and adds no UI.
2. **Link persistence and atomic pull synchronization.** Add the dedicated link schema and the
   manager operations for detached imports, read-only mirrors, conflict detection, successful
   atomic replacement, missing-server state, unlink, and removal.
3. **UI, localization, and end-to-end behavior.** Add explicit Import Copy and Keep Synced actions,
   Sync Now, status and conflict presentation, Retry/Unlink/Remove Local Copy recovery, reconnect
   refresh, and deterministic integration coverage.

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
operations. In the foundation stage, an HTTP rejection or Subsonic failed envelope is a closed
backend failure and is never reinterpreted as an empty playlist list. Subsonic-family servers do
not report unsupported endpoints and missing entities consistently enough for the generic client
to infer either state from HTTP status or error code alone. Dialect-aware unsupported/missing
classification belongs to the persistence stage, where a successful complete listing can provide
stronger evidence; until then failure retains prior state and grants no capability.

The first stage does not call create, update, or delete playlist endpoints. Adding those calls
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
result even when the server call itself succeeded. A guard from a predecessor session cannot be
reused against its successor. Adapter work runs outside the lifecycle mutex, and revalidation never
holds that mutex across network I/O or allows a selector to re-enter it.

An operation result distinguishes adapter-level unsupported authority and current
unavailable/stale authority from a closed backend failure category. Neither variant includes an
endpoint URL, credential, raw server message, response body, native ID, route, or session token.

Playlist endpoint membership is not music-catalogue authority. A track ID present only in a
playlist snapshot remains useful durable identity for a later import or mirror, but it remains
unavailable for display/playback until the normal accepted-catalogue authority independently
contains that exact `(SourceId, TrackId)`. Conversely, an accepted catalogue does not authorize a
server-playlist call.

## Future persistence boundary

The persistence stage will use a dedicated link entity rather than overloading regular-playlist
entries. One linked row may retain only the minimum non-secret synchronization identity and state:

- the local regular-playlist ID;
- the exact owning `SourceId`;
- the exact `NativePlaylistId`;
- the pull direction and linked/read-only state;
- the last successfully synchronized bounded name and ordered-membership digest;
- the last-success timestamp and a closed current status such as current, conflict, or missing.

The link must not store a server URL, endpoint path, username, credential, token, salt, stream or
artwork locator, source route, adapter object, lease, session epoch, or raw failure. The linked
regular playlist continues to store each media occurrence as canonical `(SourceId, TrackId)` under
the source-scoped storage contract.

Creating an import or mirror and applying a pull are all-or-none database operations. A failed,
cancelled, stale, oversized, or malformed pull changes neither the last complete entry snapshot nor
the last-success metadata. Import Copy removes all provisional link state before its transaction
commits. Keep Synced publishes its new entries, link metadata, digest, and status in one commit.

## Conflict and local-drift policy

The manager refuses ordinary Add, Remove, reorder, rename, and destructive entry edits while a
playlist is linked. This makes the shipped UI read-only, but persistence still verifies a digest of
the last synchronized name and exact ordered `(SourceId, TrackId)` occurrences before every pull.
The digest detects edits from an older binary, direct database access, recovery tooling, or a bug.

If current local state differs from the last successful digest, the pull changes nothing and marks
an explicit conflict. Resolution requires one deliberate action:

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

A successful reconnect schedules one bounded refresh per linked playlist after the accepted source
session is current. A manual Sync Now shares the same deduplicated operation lane. A newer request
or lifecycle retirement cancels older work; only the latest still-current generation may commit.
The initial version does not continuously poll and does not keep a session alive solely for sync.

## Server rename and deletion

For an imported copy, server rename or deletion is irrelevant because no link remains. For a
mirror, a successful detail response with the same exact native ID may update the mirrored name
along with its entries in the same atomic pull.

A playlist absent from one successful complete current listing is marked **missing on server**
without deleting or emptying the local snapshot. A failed detail call alone is not deletion
evidence because Subsonic dialects conflate missing, unsupported, and other failures; it retains the
last snapshot as a closed failure until a complete listing or later dialect-aware mapping confirms
the state. The user is offered:

- **Retry**, which keeps the link and asks the current session again;
- **Unlink**, which retains the last snapshot as an editable regular playlist; or
- **Remove Local Copy**, which explicitly deletes the local playlist and link transactionally.

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
