# Source-scoped regular playlist storage and authority contract

This document defines the durable-storage contract and the following live-catalogue authority
foundation for [P1.5](task.md#p15--persist-source-scoped-playlists). Neither foundation changes
which source rows the shipping UI can add, display, or play.

The central rule is:

> A regular-playlist occurrence identifies media by the stable pair `(SourceId, TrackId)`. A
> playlist is an ordered view over media owned by those sources; it is not itself a media source.

That pair is identity, not location. A server URL, stream URL, file URI, filesystem path, access
token, password, DAAP session ID, media lease, or source session epoch must never be substituted for
either component or persisted as remote playlist authority.

## Scope and delivery boundary

This storage slice covers:

- migration 13 and the `playlist_entries` entity;
- deterministic conversion of every valid existing entry to the built-in local `SourceId`;
- a canonical source-scoped identity plus a separate local foreign-key cache;
- preservation of playlist order, duplicate occurrences, entry identity, and local reconciliation
  evidence;
- typed storage operations for future non-local additions without admitting locators or
  credentials—the schema is source-generic, but this is an internal capability rather than a
  promise that every source kind is addable; and
- compatibility for all currently shipping local regular-playlist, XSPF-import, reconciliation,
  rename, and deletion paths.

It deliberately does **not** enable remote Add to Playlist, mixed-source playlist rendering or
playback, disconnected-row presentation, playlist-UI lifecycle refresh, or remote XSPF export.
P1.5 Record A adds the internal live-registry authority described below; Record B must integrate it
into those user-facing behaviors. Subsonic server-native playlist listing, import, synchronization,
conflict handling, and deletion semantics remain a separate record and require their own design
before implementation.

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

The storage boundary accepts typed `SourceId` and `TrackId` values. A future non-local insertion
may retain optional non-secret metadata fingerprints, must not provide a file path, and must commit
all selected occurrences atomically. Those normalized snapshots are neither display fields,
identity, nor matching authority. The exact same native track ID from two sources remains two
different media objects.

The user-visible behavior in this slice stays local-only:

- **Manual local add:** writes `(SourceId::local(), tracks.id)` and the matching
  `local_track_id`, preserving normalized fingerprint metadata and duplicate occurrences.
- **Regular-playlist load:** retains the established local projection and ordering. Stored
  non-local or currently unmatched entries are not yet rendered or sent to playback.
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
- **XSPF export:** remains the existing local-track export in this slice. Mixed-source export needs
  an explicit metadata-only policy and is part of the later presentation/integration work; it may
  not obtain or serialize a protected remote locator.

The existing P1.2 refusal remains correct until Record B lands: selecting Add to Playlist
from a remote, radio, removable, external, or malformed source still presents localized all-or-none
copy before opening the database. Neither the storage capability nor Record A's internal lookup is
itself UI authorization. Record B will initially admit only retained authenticated catalogues
through the explicit registry capability. Radio-Browser, removable media, ephemeral external
files, and unknown sources remain unsupported until their persistence and lifecycle semantics are
separately designed.

## Live registry and accepted-catalogue authority

P1.5 Record A establishes an internal authority boundary between durable source-scoped identity
and later playlist UI integration ([#141](https://github.com/jm2/tributary/pull/141)).
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

These APIs are an authority foundation only. The shipping Add/Remove/render/Play paths do not call
them yet, do not show stored non-local rows, and retain the localized all-or-none refusal from P1.2.

## Source lifecycle and unavailable identity

Persistent membership does not imply a live source session. Only `source_id` and `track_id` survive
restart; a session epoch is deliberately transient.

Record A can now validate a non-local identity internally against the exact current
`SourceRegistry` source, epoch, accepted catalogue generation, and native track. Record B must use
that result to make disconnect, source retirement, a missing server-side track, refresh, or session
replacement visibly unavailable while leaving the database row intact. Reconnection may restore it
only when the same `SourceId` publishes the same `TrackId`; endpoint similarity and metadata are
not identity evidence.

Until Record B exists, stored non-local fixtures remain intentionally outside the regular-
playlist UI projection. This prevents the storage migration from accidentally treating durable
identity as a locator or presenting a row that the current queue still assigns to the local source.

## Ratings and playback history

This migration does not transfer ownership of track metadata:

- Tributary ratings remain writable only for tracks owned by the local library. A future remote
  playlist row will display the live source's read-only or unsupported rating capability; this
  schema persists no rating snapshot and grants no write authority.
- Playback history remains local-only. A local occurrence reached through a regular playlist can
  still contribute to its exact local track. A future remote occurrence must not update the local
  `tracks` table merely because it appears beside local entries.

See the [rating contract](ratings.md), [playback-history contract](playback-history.md), and
[source-lifecycle architecture](architecture/source-lifecycle.md) for those independent authority
boundaries.

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

Mixed-source rendering, interaction, playback, unavailable-state presentation, and native
Subsonic playlist tests belong to their explicitly deferred records rather than being claimed by
the storage or authority foundations.

Record A additionally requires automated coverage for default-deny adapters, the four explicit
authenticated opt-ins, Invalid playlist indexing for missing/duplicate catalogue-native identity,
sanitized metadata, stale epoch/generation rejection, predecessor retention during connecting or a
failed replacement, invalidation after replacement/refresh, synchronous disconnect/shutdown/final-
release denial, and pre/post-async stream and artwork revalidation. Ordered lookup must preserve
duplicate requested occurrences and isolate one missing track's Unavailable result from valid
neighbors. Passing those tests does not claim Add/Remove/render/Play UI integration.
