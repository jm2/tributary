# Source-scoped regular playlist storage contract

This document defines the durable-storage foundation for the first record in
[P1.5](task.md#p15--persist-source-scoped-playlists). It changes how regular-playlist membership is
represented without yet changing which source rows the shipping UI can add, display, or play.

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
playback, disconnected-row presentation, lifecycle refresh, or remote XSPF export. Those behaviors
need the live `SourceRegistry` and remain the second P1.5 record. Subsonic server-native playlist
listing, import, synchronization, conflict handling, and deletion semantics remain the separate
third record and require their own design before implementation.

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

The existing P1.2 refusal remains correct until the second record lands: selecting Add to Playlist
from a remote, radio, removable, external, or malformed source still presents localized all-or-none
copy before opening the database. The new storage capability is not itself UI authorization.
That follow-up will initially admit only retained authenticated catalogues through an explicit
capability check. Radio-Browser, removable media, ephemeral external files, and unknown sources
remain unsupported until their persistence and lifecycle semantics are separately designed.

## Source lifecycle and unavailable identity

Persistent membership does not imply a live source session. Only `source_id` and `track_id` survive
restart; a session epoch is deliberately transient.

The second P1.5 record will resolve a non-local entry by asking the live `SourceRegistry` for the
exact source and track, adopting only the current non-secret session epoch at publication and
playback time. Disconnect, source retirement, a missing server-side track, or an epoch replacement
must leave the database row intact and make it visibly unavailable. Reconnection may restore it
only when the same `SourceId` publishes the same `TrackId`; endpoint similarity and metadata are
not identity evidence.

Until that slice exists, stored non-local fixtures remain intentionally outside the regular-
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

Mixed-source rendering, interaction, playback, lifecycle, and native Subsonic playlist tests belong
to their explicitly deferred records rather than being claimed by this storage foundation.
