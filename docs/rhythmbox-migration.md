# Rhythmbox migration contract

- Status: implemented P2.1 contract for [issue #57](https://github.com/jm2/tributary/issues/57)
- Contract version: 1
- Last reconciled with the implementation: 2026-07-20

Tributary provides an explicit, one-time migration from a user-selected Rhythmbox profile. It can
import regular and exactly representable automatic playlists, merge selected application-owned
metadata, and retain unmatched regular-playlist entries as path-only intent. This is separate from
XSPF import because it can change local track metadata and therefore requires a preview and an
atomic commit.

The implementation accepts `rhythmdb.xml` with a `rhythmdb` root whose `version` attribute is
exactly `2.0`, and an optional `playlists.xml` with a `rhythmdb-playlists` root. Both documents use
XML 1.0 and UTF-8. A different catalogue version or either different root fails closed.

## Scope

The migration can:

- match Rhythmbox `song` entries to the built-in local library by exact decoded path;
- apply one explicit old-root to new-root path mapping before that comparison;
- monotonically merge play counts and, when selected, last-played timestamps;
- fill ratings or explicitly replace conflicting local ratings;
- create every conflict-free static playlist, preserving its retained occurrence order and
  duplicates;
- create every automatic playlist whose complete query belongs to the exact subset below; and
- store a content-redacted receipt for the parsed source semantics and import policy.

It does not:

- discover profiles, run at startup, monitor Rhythmbox, or synchronize later edits;
- create or scan tracks, copy media, repair tags, or write audio-file metadata;
- use titles, artists, albums, duration, inode identity, case folding, Unicode normalization, or
  any other fallback to identify a track;
- mutate remote or source-owned tracks and playlists;
- import queues;
- approximate an unsupported automatic playlist or materialize its current members; or
- offer per-playlist or per-row selection. The preview covers one all-safe-items plan.

Only Tributary's built-in local database is mutated. The separate contracts for
[ratings](ratings.md), [playback history](playback-history.md), and
[source-scoped playlists](source-scoped-playlists.md) continue to apply.

## User workflow and safe-subset acknowledgement

**Import from Rhythmbox…** is a separate application action, not another format in the ordinary
playlist importer.

1. Before reading a profile, the user chooses whether to import ratings, replace rating conflicts,
   import play counts, import last-played history, and apply an optional root remap. Ratings and
   play counts default on. Rating conflicts default to keeping Tributary's value. Last played
   defaults off.
2. The user selects a local directory. Tributary reads only its direct `rhythmdb.xml` child and,
   when present, its direct `playlists.xml` child. `rhythmdb.xml` is required; `playlists.xml` is
   optional.
3. A bounded worker captures, parses, and plans against a read-only view of the current local
   database. The preview shows aggregate changes and the bounded actionable report described
   below.
4. The plan automatically includes all safe track updates, conflict-free supported playlists, and
   retained path-only static occurrences. It applies the displayed rating-conflict policy and
   omits unsafe or unsupported source items; the UI does not imply that an omission was imported.
5. If any report section is non-empty, Apply remains unavailable until the user explicitly accepts
   the shown safe subset, including its conflict resolutions, preserved path-only occurrences, and
   skipped items. This acknowledgement does not turn an omitted item into an approximate import.
6. Apply transfers the opaque request once to the serialized local-library command lane. The
   result is `Applied`, committed-but-refresh-incomplete `AppliedRefreshFailed`, `AlreadyApplied`,
   `Stale`, or a closed failure state.

The acknowledgement is consent for the already-planned safe subset, not a result-affecting policy.
It is therefore not part of the receipt key. Changing what should be imported requires changing the
source snapshot, root mapping, or metadata policy and creating a new preview.

## Coherent and bounded input capture

The accepted child names are fixed, and the selected profile must itself be a direct local
directory rather than a symlink or Windows reparse point. The capture layer:

- stamps the selected directory's stable identity before opening children and rechecks it after
  both opens and after both document reads;
- rejects child symlinks, non-regular files, and Windows reparse points;
- opens and stamps both present documents before reading either one;
- retains the open handles and reads in 64 KiB chunks with a maximum-plus-one size check;
- records stable platform file identity, length, and modification time;
- rechecks each retained handle and reopens each pathname after both reads;
- rechecks that an initially absent optional `playlists.xml` is still absent; and
- rejects the complete preview if either document is replaced, truncated, extended, or observably
  modified during capture.

Platforms without a supported stable file-identity implementation fail closed. One global worker
gate permits only one full capture/parser/planner job at a time, so superseded large previews cannot
accumulate concurrently.

The documents are bounded in-memory byte buffers. The XML reader then processes them as events; the
implementation does not claim constant-memory streaming.

## XML and parser boundaries

The parser accepts an optional XML declaration only when it declares XML 1.0 and, if present, UTF-8.
It rejects malformed UTF-8 or XML 1.0 characters, DTDs, custom entities, processing instructions,
qualified/prefixed names, multiple roots, trailing content, invalid nesting, and duplicated relevant
track scalar fields. The five predefined XML entities and numeric character references are decoded
and then checked against the same character and text limits. No entity, schema, stylesheet, network,
or filesystem expansion occurs.

The v1 resource ceilings are:

| Resource | Maximum |
|---|---:|
| `rhythmdb.xml` bytes | 128 MiB |
| `playlists.xml` bytes | 64 MiB |
| XML elements | 4,000,000 per document |
| XML depth | 16 |
| Attributes | 32 per element |
| Song records | 250,000 |
| Playlists | 10,000 |
| Static/queue location occurrences | 500,000 total |
| Automatic-query nodes | 256 per playlist |
| One retained text/name/attribute value | 256 KiB |
| Total retained decoded text | 128 MiB |
| Item-level issues | 100,000 |
| One root or mapped path | 64 KiB of UTF-8 |
| Planner mapped-path budget | 128 MiB, charged per valid song and considered static occurrence |

Checked arithmetic enforces all ceilings. A structural, document, or resource-limit failure rejects
the whole preview. A bad location, unsupported entry type, unsupported playlist type, or invalid
rating/count/last-played value is instead a typed item-level issue. An invalid numeric field is
discarded without discarding the same row's valid location or other numeric fields.

The byte buffers, XML reader state, decoded model, planner collections, and bounded report overlap
during preview. The production limits therefore bound both individual inputs and the aggregate
transient working set instead of assuming that each phase's maximum allocation exists alone.

## Exact paths and root remapping

Only an `<entry type="song">` with one valid local `file:` location can participate in track
matching or metadata transfer. Other entry types are reported and ignored.

A location is accepted only when it:

- parses as a `file:` URI with an empty authority or `localhost`;
- has no credentials, port, query, or fragment;
- has valid percent escapes and decodes to an absolute, UTF-8 path for the running platform; and
- contains no NUL or parent-traversal component, including traversal hidden by percent-encoded
  dots or separators.

The URI spelling is never compared to the database. Tributary performs no filesystem
canonicalization, symlink resolution, case folding, Unicode normalization, basename search, or
alternate-root search.

An optional remap consists of two different absolute UTF-8 paths whose raw retained spelling
contains no `.` or `..` path segment; even an interior or trailing dot segment that the platform
would normalize away is rejected. Each root and mapped result is limited to 64 KiB of UTF-8, and
each valid parsed song, plus each valid occurrence in a conflict-free static playlist considered
for creation, is charged against the plan's checked 128 MiB mapped-path budget before another clone
is retained. Queues, invalid occurrences, and playlists already skipped for a name conflict retain
no mapped paths in the plan. For each decoded source path it performs exactly:

```text
if source_path has old_root as a path-component prefix:
    mapped_path = new_root + suffix_after_old_root
else:
    mapped_path = source_path
```

The mapping is lexical, component-aware, and applied once. It is not inferred and the destination
is not silently rewritten to a configured library root.

After mapping, the platform path string must equal one current `tracks.file_path` value byte for
byte. Zero matches means unmatched. Multiple Rhythmbox song rows that collapse to the same mapped
path are reported as a duplicate-location group and excluded from metadata planning; they are not
merged heuristically.

Apply revalidates every relevant path-to-track result and the expected path, play count,
last-played value, and rating of every referenced local row. It also rechecks incoming playlist
names. If that relevant current state no longer equals the preview evidence at Apply, the request
returns `Stale` instead of silently changing the plan. A change reverted exactly before Apply is
indistinguishable from unchanged equivalent state and is safe to admit.

## Static playlists

Playlist-name admissibility is the first playlist-level decision, before source kind or contents.
An empty, already-existing, or source-duplicate name produces one playlist-name-conflict result and
skips that complete playlist; it does not also produce queue, unsupported-rule, or static-occurrence
details. This first-applicable-reason precedence keeps report categories deterministic.

Every conflict-free `type="static"` source playlist becomes a local regular playlist. Its required
`name` attribute is used exactly. An empty name, an exact name already present in Tributary, or a
name repeated in the source is reported and that complete playlist is skipped. Existing playlists
are never overwritten, appended to, renamed, or claimed as migration-owned.

For each created static playlist, the importer preserves:

- the relative order of all retained source occurrences;
- every duplicate occurrence, including adjacent duplicates; and
- an empty source playlist as an empty local playlist.

An exactly matched occurrence stores the local source and track identity. A valid but unmatched
occurrence is retained in the same position as an unavailable local, path-only entry: both track-ID
fields are absent, metadata fingerprint fields are empty, and `match_file_path` contains the exact
decoded/remapped path. Existing local reconciliation can later resolve that intent by exact path;
there is no title/artist/duration fallback.

An invalid or non-local location cannot safely become path-only intent. It is reported and omitted;
later retained entries keep their relative order but database positions are compacted. A queue
whose name passed the earlier check is reported with its source ordinal, name, and occurrence count
and is always skipped.

## Automatic playlists

An automatic playlist is created as a live Tributary smart playlist only when its complete
attributes and query belong to the closed subset below. Any unsupported attribute, query node,
predicate, limit, or sort rejects that entire playlist. The importer never drops only the unknown
part and never captures current members as a static substitute.

Current Rhythmbox writes three common source-presentation attributes before serializing every
settings-backed playlist. `show-browser` is accepted only as `true` or `false`,
`browser-position` only as the canonical signed 32-bit decimal emitted by Rhythmbox, and
`search-type` as one bounded XML string. They restore Rhythmbox browser layout/search-control UI
state and do not participate in query membership, so Tributary validates and otherwise ignores
them. Valid instances are also excluded from semantic receipt identity: changing only that
Rhythmbox UI state does not manufacture a new migration attempt. A malformed presentation value
or any other unknown playlist attribute makes that automatic playlist
unsupported and remains part of the digest. A duplicate XML attribute is rejected earlier as a
malformed document; the translator's duplicate guard is only a defense for constructed internal
models.

### Query shape

The accepted serialized shape has exactly:

1. one attribute-free outer `conjunction` query root;
2. exactly two children in order: `equals prop="type"` with text `song`, then one attribute-free
   `subquery`;
3. exactly one attribute-free inner `conjunction`; and
4. one or more direct predicates, with either no separators (`MatchMode::All`) or an alternating
   `predicate`, empty `disjunction`, `predicate` sequence (`MatchMode::Any`).

Nested groups, mixed Boolean shapes, extra guards, child-bearing predicates, extra predicate
attributes, and text with surrounding whitespace are unsupported.

### Predicates

Only `play-count` and `rating` are supported.

`play-count` accepts an ASCII decimal integer in `0..=i32::MAX` with `equals`, `not-equal`,
`greater`, or `less`. Rhythmbox's `greater` and `less` comparisons are inclusive; the translator
shifts the integer threshold so Tributary's strict numeric operators preserve that boundary.

`rating` accepts a finite value in `0.0..=5.0`:

- `equals 0` maps to unrated;
- `not-equal 0` maps to rated;
- positive `equals` and inclusive `greater` values are supported on the exact 0.05-star grid;
- positive inclusive `less` is supported only when the surrounding Boolean expression separately
  guarantees that every candidate is rated; and
- positive `not-equal`, `greater 0`, and `less 0` are rejected because Rhythmbox and Tributary do
  not classify missing ratings identically.

When a numeric rating predicate is used, every positive rating in the complete parsed source
catalogue must also lie exactly on the 0.05-star grid. This prevents the metadata conversion from
changing equality or threshold membership. Text, date, duration, year, track/disc number, bitrate,
file-size, and all other properties remain unsupported.

### Limits and ordering

An absent limit is supported. One ASCII-decimal `limit-count` attribute that parses to zero is also
accepted as inert. Any active count limit and any appearance of `limit-size`, `limit-time`, or
alternate `limit` is unsupported. Tributary cannot reproduce Rhythmbox's source-location cutoff
tie-break exactly.

Every explicit `sort-key` is rejected, including otherwise familiar numeric fields, because the
same tie-break mismatch can change ordering. A `sort-direction` without an accepted sort is also
rejected. Imported automatic playlists therefore have no limit and no explicit sort order.

## Metadata policies

Metadata is proposed only for one exactly matched local track. Invalid or absent source metadata
does not block playlist migration for that source location.

### Play counts

`play-count` must be a non-negative integer no greater than `i32::MAX`. When import is enabled:

```text
committed_play_count = max(existing_play_count, incoming_play_count)
```

The value is absolute, not a delta. Missing or invalid values and incoming values that do not
increase the local count produce no write.

### Ratings

A source rating must be finite and in `0.0..=5.0`. Missing and zero mean no proposed write; they
never clear an existing Tributary rating. A positive value converts to Tributary's `1..=100` scale
by rounding `rating * 20` to the nearest integer and clamping that positive result to the scale.

The default policy fills only an unrated local track. An equal existing rating is a no-op and a
different rating is reported while Tributary's value is kept. **Replace conflicting ratings**
instead reports and writes the converted Rhythmbox value. Both kept and replaced conflict counts
are shown separately.

### Last played

Last-played import is independently selected and off by default because it affects Recently Played,
history-based smart rules, and recency ordering. The source is a non-negative whole Unix timestamp
in seconds that must convert to a Chrono-representable UTC millisecond value. Zero and missing mean
no write. When enabled:

```text
committed_last_played = max(existing_last_played, incoming_last_played)
```

This does not create a playback event or increment play count.

## Actionable report

The preview has nine independently bounded detail sections:

1. parser issues;
2. unmatched source tracks;
3. duplicate mapped source locations;
4. kept or replaced rating conflicts;
5. playlist-name conflicts;
6. skipped queues;
7. unsupported source or automatic playlists;
8. invalid static-playlist occurrences; and
9. unmatched path-only static-playlist occurrences.

Each section retains at most 100 detail rows and separately reports the exact number omitted from
display. Aggregate summary counts still cover the complete bounded plan; the source-track
aggregate means valid parsed local-file song rows, not raw `<entry>` elements rejected for a
missing or invalid location. Path-derived detail is generated in deterministic mapped-path order.
Source-derived playlist and issue detail retains source ordinals. The report provides the path,
playlist name,
source/entry ordinal, count, or closed reason needed to understand an omission or conflict, while
remaining bounded to at most 900 rows.

Any retained or omitted detail in any section triggers the single safe-subset acknowledgement.
A clean preview needs no acknowledgement.

## Semantic identity and receipt

The source identity is a domain-separated SHA-256 digest of parsed migration semantics, not raw XML
bytes. The canonical encoding:

- distinguishes present from absent `playlists.xml`;
- sorts song records by decoded location and parsed metadata, so irrelevant source record order is
  ignored while duplicate rows remain represented;
- preserves playlist order, kind, names, static/queue occurrence order and duplicates, automatic
  query structure, and typed issue ordinals/reasons; and
- excludes valid membership-inert `show-browser`, `browser-position`, and `search-type` playlist
  settings, and otherwise uses framed fields and sorted XML attributes so formatting, comments,
  attribute order, and equivalent source spelling that parses to the same semantics do not
  manufacture a new attempt.

A separate domain-separated policy digest covers rating import and conflict behavior, play-count
import, last-played import, and the exact optional remap roots. The importer version is the third
identity component.

The `rhythmbox_import_receipts` primary key is exactly:

```text
(snapshot_digest[32], importer_version, policy_digest[32])
```

The strict `WITHOUT ROWID` table contains only those three fields. It has no timestamp, pathname,
playlist name, track ID, metadata value, acknowledgement, XML, generated-object identity, foreign
key, trigger, or extra index. Startup revalidates the critical table shape even when the migration
ledger is already current. Downgrade drops only an empty exact table; any retained receipt makes a
lossless downgrade impossible, so it is refused without deleting data.

Preview detects an existing exact receipt, and Apply checks again before any mutation. An exact
semantic-and-policy retry returns `AlreadyApplied`, including after restart, and publishes no
library refresh. Raw XML edits that do not change parsed semantics remain the same attempt.

Because the receipt proves that the safe subset committed, resolving a previously reported item
only in Tributary's local database does not make the same attempt run again. A new import attempt
requires changed parsed source semantics or a changed result-affecting policy/remap. Path-only
playlist entries may instead reconcile later through the ordinary exact-path mechanism.

## Transaction, publication, and lifecycle

Apply runs in the same serialized FIFO as local rating and playback-history writes. One database
transaction:

1. checks the exact receipt;
2. reloads local tracks and revalidates every expected path match and referenced track state;
3. rechecks the expected presence or absence of every unique incoming playlist name;
4. writes planned track metadata updates;
5. creates planned regular/smart playlists and retained regular occurrences;
6. inserts the receipt last; and
7. commits.

Any validation or storage failure rolls back every metadata, playlist, occurrence, and receipt
write. A concurrent process that wins the identical receipt race is recognized only after the
losing transaction rolls back and is returned as `AlreadyApplied`.

There is no optimistic GTK mutation. After a successful first commit, the engine independently
attempts one complete local-library `FullSync`, one playlist-projection invalidation, and a sidebar
refresh request. When all three are admitted, the completion is `Applied`. If the database commit
succeeds but any refresh leg cannot be completed while the current GTK event lane remains open, the
content-free completion is instead `AppliedRefreshFailed`; the dialog says that the durable changes
committed and directs the user to restart so it never falsely describes a rollback. A window event
lane already closed for shutdown is intentionally inert, while the FIFO still drains through its
Flush barrier. An `AlreadyApplied`, stale, rejected, or rolled-back request initiates none of those
three refreshes. Every completion carries only its request UUID, closed outcome, and content-free
summary.

Once a request is admitted, a non-closeable applying dialog is retained under that request UUID
until the matching completion arrives. A second migration wizard is refused during this state.
Completions for any other or obsolete request generation are ignored.

Cancelling or superseding a preview sets its generation token. Capture observes cancellation
between 64 KiB reads and each major phase. Parsing and database planning are bounded but are not
interruptible in the middle of a pass; cancellation is checked immediately before and after them,
and a late result from an obsolete generation is discarded on the GTK thread.

Closing the window synchronously revokes chooser/preview UI ownership before command admission is
closed. A request already admitted to the engine is not cancelled: it reaches commit or rollback
before the shutdown FIFO flush marker completes. Since window-scoped event reception is closed,
its late completion cannot access destroyed GTK state.

## Privacy and diagnostics

Paths, remap roots, playlist names, locations, source XML, track IDs, metadata values, and digests
are private. Actionable details may appear in the local preview, while imported playlist names,
path-only intent, and metadata necessarily appear in the ordinary library tables. Migration does
not write a temporary report or a second provenance copy. The preview renders control and
bidirectional-control
characters as visible Unicode escapes so private names and paths cannot spoof surrounding UI text.

Migration model and error `Debug` implementations redact paths, names, query text, metadata values,
prepared operations, digest bytes, and underlying storage details while preserving the internal
error source for typed handling. Report sections disclose only retained/omitted counts in `Debug`.
Parser and completion errors use closed categories. Engine logs contain a request UUID, closed
category, and aggregate counts; raw filesystem/XML/database error chains and private detail are not
logged or sent to GTK.

The prepared operations and digest remain private fields of the opaque request. GTK can inspect the
bounded report for local display and can transfer that request once, but cannot rewrite its path
matches, track state, playlist contents, or receipt identity before Apply.
