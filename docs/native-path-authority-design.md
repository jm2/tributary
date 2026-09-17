# Native local-path authority design (R11 / #258)

Status: design contract, no implementation in this bead. This document is the
deliverable the R11 acceptance calls for: a versioned reversible native-path
representation plus an explicit unsupported-input boundary, for review before
any behavior changes. It authorizes follow-up implementation beads; it does not
ship them.

Revision 2 (this head) is a corrective revision of the independently rejected
head `16140be0d03d17c1f299cf7690adea6648722160`. It withdraws the size/mtime
legacy-adoption rule (F1), makes the rollout fail-closed-first so no authority
consumer is ever exposed to a row it cannot prove (F2), and removes the
older-binary compatibility claim in favor of an enforced version guard plus a
mechanical carrier removal (F3). §13 is the finding-to-revision map. No
behavior changes: the revision makes the contract stricter, not more permissive.

Scope: the built-in local library only. No change to removable media,
SourceId, remote adapters, or shared city policy. Scratch and fixture paths use
`${TMPDIR:-/var/tmp}`.

Related records:

- Tracked issue: [jm2/tributary#258](https://github.com/jm2/tributary/issues/258).
- Task index: `docs/task.md` R11.
- Tag-write authority boundary owned by R1:
  [jm2/tributary#248](https://github.com/jm2/tributary/issues/248).
- Existing path-free identity contracts: `docs/architecture/source-lifecycle.md`.
- Removable lossless encoding precedent: `src/architecture/identity.rs`.

## 1. Summary of the decision

Adopt a **versioned, reversible native-path key** as the authoritative
local-library locator, and keep the current lossy string as **display text
only**. Where exact identity cannot be proven for pre-existing rows, **quarantine
those rows** (retain the row and its history, refuse playback/tag-write/export
authority) rather than guessing. Where a path cannot be represented at all,
**refuse it at the boundary with a closed diagnostic** instead of silently
storing a false locator.

Two decisions are made here:

1. **Lossless representation (primary).** A new `tracks.native_path` column
   stores the exact native path encoded with the same platform-tagged,
   hex-frozen spelling already used for removable media
   (`unix:` / `windows-utf16le:` / `portable-utf8:`). It is reversible
   (decode reproduces the exact native bytes/code units) and self-describing
   (the scheme tag is the version). The existing legacy locator column is
   **renamed** `tracks.file_path` → `tracks.display_path`; it becomes
   non-authoritative display text, loses its UNIQUE constraint, and is never
   used to open a file, key a row, or prove identity. The rename is deliberate:
   it removes the legacy authority carrier so that a pre-R11 binary fails closed
   instead of silently regaining lossy authority over the upgraded database
   (§4.5).
2. **Explicit unsupported-input boundary (secondary, permanent).** A local
   library **root** must itself be a valid UTF-8 absolute path. Roots are the
   one object whose stored key (`library_roots.path`) is not part of this
   contract; refusing a non-UTF-8 root keeps that key honest. Descendant
   (leaf/directory) names with invalid bytes remain fully supported and
   lossless. Independently, any native path that cannot be losslessly encoded
   (over the size bound, unsupported scheme) is refused with a closed
   diagnostic and is never indexed with a lossy key.

Until the representation is authoritative in code, the same boundary applies in
its interim form: a second distinct native path whose lossy spelling equals an
already-indexed row's display text must be **refused and diagnosed**, never
collapsed into that row, and a stored row whose display text contains the
replacement character U+FFFD must not be used as playback, tag-write, stale-
removal, or export authority.

## 2. Problem and current behavior

The production tag parser persists `path.to_string_lossy().to_string()` as
`ParsedTrack.file_path` (`src/local/tag_parser.rs:169`). Every scanner and
watcher insert/update funnels that value into `tracks.file_path` through
`apply_parsed_track_fields` (`src/local/engine.rs:6367`) and `upsert_track`
(`src/local/engine.rs:6234`). The schema declares that column `NOT NULL UNIQUE`
(`src/db/entities/track.rs:12-13`; `src/db/migration/m20250101_000001_create_tables.rs:18-23`).

On Unix, `to_string_lossy` maps every invalid byte to U+FFFD. Consequences:

- **Collision.** `a\xff.flac` and `a\xfe.flac` both persist as
  `a\u{FFFD}.flac`; the second insert violates the unique index.
- **Literal replacement-character collision.** A real UTF-8 file literally
  named `a\u{FFFD}.flac` is indistinguishable in storage from either invalid-
  byte file above.
- **Wrong-file authority.** Playback reconstructs the locator with
  `PathBuf::from(&model.file_path)` (`src/local/resolver.rs:353`) and opens the
  bytes of the lossy string. If that happens to name a different real file, the
  wrong file is opened; if it names none, playback fails although the file
  exists.
- **Wrong stale-removal / membership decisions.**
  `on_disk_paths: HashSet<String>` and `should_remove_stale_track`
  (`src/local/engine.rs:2318-2329`, built at `3889-3893`) compare lossy
  strings, so a surviving different file can keep a dead row alive and a real
  file can be treated as absent.
- **Rename refused.** The scanner's own directory-rename path rejects a batch
  when two distinct native names collapse to one key
  (`src/local/engine.rs:2175-2190`, error at `2187`).
- **Tag writes target the wrong file.** The tag-write target is reconstructed
  from the lossy display string through the UI URI chain
  (`src/ui/window.rs:4622-4624` → `TrackObject.uri` → `local_file_path` in
  `src/ui/context_menu.rs:1349-1354` → `SaveTarget::LocalPath`).
- **Import/export collapses again.** XSPF export writes
  `file_path_to_uri(&t.file_path)` (`src/local/playlist_io.rs:89,732`) from the
  lossy string, and XSPF import decodes back with
  `to_file_path().to_string_lossy()` (`:766`). Rhythmbox import refuses
  non-UTF-8 locations outright (`src/local/rhythmbox_import.rs:1534`,
  `RhythmboxLocationIssue::NotUtf8`).

The codebase already acknowledges the defect in `rename_directory_rows`
(`src/local/engine.rs:6515-6521`): "Rows are persisted through
`to_string_lossy`, so a non-UTF-8 name never round-trips back to its original
bytes."

### 2.1 What is already correct

- **Track identity is not the path.** A local track's native `TrackId` is the
  SQLite `tracks.id` UUID (`src/local/engine.rs:6668-6681`), and queue/playlist
  references key on that id (`playlist_entries.local_track_id`, FK
  `fk_entry_local_track` → `tracks(id)` ON DELETE SET NULL). Ratings, play
  counts, `last_played_at_ms`, and history live on the same row. Preserving the
  row therefore preserves all of that automatically.
- **A lossless codec already exists.** `src/architecture/identity.rs` defines
  the exact reversible spelling used for removable media:
  `unix:<hex(bytes)>`, `windows-utf16le:<hex(LE u16)>`,
  `portable-utf8:<hex>` (`identity.rs:279-306`), with a decode path and a
  re-encode canonicality check (`:202-265`) and a size bound
  (`MAX_TRACK_ID_BYTES`, `:23,308-317`). R11 reuses this spelling rather than
  inventing a second one.

The fix is therefore a **locator/key** fix, not an identity rewrite. That keeps
the change bounded and preserves history by construction.

## 3. Contract: representation

### 3.1 Spelling and versioning

The authoritative key is a self-describing string:

```text
native_path := scheme ":" payload
scheme      := "unix" | "windows-utf16le" | "portable-utf8"
payload     := lowercase hex of the native code units
```

- Unix: `payload = hex(path.as_os_str().as_bytes())`.
- Windows: `payload = hex(path.as_os_str().encode_wide().flat_map(u16::to_le_bytes))`.
- Other: `payload = hex(path.to_str()?.as_bytes())`; a non-UTF-8 path on a
  platform with no native byte view is refused, not lossily stored.

The scheme tag is the format version. A future incompatible encoding **must**
introduce a new scheme token (for example `localpath-v2:`) and must not
reinterpret an existing one. Decoders are total over known schemes and
**fail closed** (quarantine) on an unknown or malformed scheme; they never
fall back to `display_path`.

Canonicality is enforced by decode-then-re-encode equality, mirroring
`identity.rs:260`: a stored key that does not re-encode byte-for-byte is
rejected. This rejects uppercase hex, odd-length payloads, embedded NUL, and
platform-mismatched schemes.

### 3.2 Absolute-path variant

`identity.rs` currently encodes *relative* removable paths and rejects
`RootDir`/`Prefix` components. R11 needs **absolute** local paths, so the codec
gains a sibling entry point (working name `NativePath::encode_absolute` /
`decode_absolute`) that:

- permits a leading root (Unix `/`) and a Windows drive/UNC prefix;
- still rejects `ParentDir` traversal only where a consumer requires a
  relative subtree, not for the absolute locator itself;
- applies the same lower-hex payload, NUL rejection, and size bound.

The two entry points share the same `encode_hex`/`decode_hex`/scheme helpers,
extracted from `identity.rs` into a small shared module so there is exactly one
spelling in the tree. The existing `TrackId::removable_relative*` behavior and
its tests are unchanged.

### 3.3 Bounds

Reuse `MAX_TRACK_ID_BYTES = 256 * 1024` (`identity.rs:23`). The bound is checked
on the **encoded** length before any write. An over-bound path is refused with
a closed diagnostic and is not indexed. This keeps a single, already-documented
ceiling rather than a second policy.

### 3.4 Display text is not identity

- `tracks.display_path` (renamed from `file_path`) is **display text**: a lossy,
  possibly duplicated human-readable rendering used for presentation, search,
  and fallback labels. It is never used to open a file, to key uniqueness, or
  to prove identity, and the physical name change means a legacy binary can no
  longer reach it as an authority carrier (§4.5).
- `tracks.native_path` is the **authoritative locator and uniqueness key**.
- The UI may show the lossy rendering (two colliding files legitimately render
  identically; honesty beats fake disambiguation). Any code that needs to
  *act* on the file must go through `native_path` decoded under retained root
  authority.

## 4. Contract: schema and migration

Follow the migration conventions in `src/db/migration/` (`m<YYYYMMDD>_<6-digit>_<name>.rs`,
registered in `mod.rs`). The next free slot is `000021` after
`m20260903_000020_lastfm_policy.rs` (`src/db/migration/mod.rs:24,51`).

### 4.1 New columns and the removed legacy carrier

- `tracks.native_path TEXT NULL` — the encoded authoritative locator. `NULL`
  means "not yet proven" (legacy quarantine) and nothing else.
- `tracks.native_path_state INTEGER NOT NULL DEFAULT 0` — a closed enum:

  | Value | Name | Meaning |
  |---|---|---|
  | 0 | `legacy_unresolved` | Column default; a keyless row with no proof; no authority |
  | 1 | `authoritative` | `native_path` is encoded, canonical, and current |
  | 2 | `legacy_verified_utf8` | Backfilled from a U+FFFD-free display string |
  | 3 | `quarantined_ambiguous` | Decode failed, collision, or unprovable legacy |

  States 1 and 2 grant authority because both carry an exact, canonical
  `native_path`; state 2 additionally guarantees its `display_path` is an exact
  round-trip, so legacy display-based consumers remain correct for it until
  they are upgraded. States 0 and 3 grant no authority. State 2 is promoted to
  state 1 by an exact re-observed key (§4.4, §5.4).

- `tracks.native_path_scheme TEXT NULL` — optional denormalized scheme for
  cheap filtering/validation. It must equal the decoded scheme of
  `native_path`; `NULL` when `native_path` is `NULL`.
- `tracks.file_path` is **renamed `tracks.display_path`** and keeps its NOT NULL
  display text, but is no longer unique and is no longer an authority carrier.
  The rename matters: see §4.2 and §4.5. No column named `file_path` survives
  the migration.

### 4.2 Uniqueness moves to the native key, and the legacy carrier is removed

`tracks.file_path` must lose its UNIQUE constraint **and its name**: two
distinct native paths may legitimately render to the same display text, and
both must be storable and visible. The rebuilt table carries `display_path`
(display, plain indexed, never unique, never authority), `native_path` (the
authoritative locator), and a unique index on `native_path` (partial,
`WHERE native_path IS NOT NULL`).

SQLite cannot drop a UNIQUE constraint in place, so the migration performs a
table rebuild following the existing raw-SQL migration style used by migration
13 and 20:

1. `PRAGMA foreign_keys` is off for the rebuild window (or the table is rebuilt
   with `legacy_alter_table` semantics), because
   `playlist_entries.local_track_id` references `tracks(id)`.
2. Create `tracks_new` with the full current column set, the renamed
   `display_path` column, and the three new native-path columns; `id` stays the
   primary key and there is **no `file_path` column**.
3. Copy every row, computing the new columns in the migration's Rust loop (hex
   encoding is not expressed in plain SQL): copy `file_path` into
   `display_path`; a U+FFFD-free `display_path` gets its encoded `native_path`
   and state 2; any other value gets `NULL` and state 3. Row ids, ratings, play
   counts, `last_played_at_ms`, `date_added`, `date_modified`, and
   `file_size_bytes` are copied verbatim.
4. Drop `tracks`, rename `tracks_new` to `tracks`, recreate the artist/album/
   genre indexes, the `display_path` display index, and the new partial unique
   `native_path` index.
5. Re-enable foreign keys and run `PRAGMA foreign_key_check`; a non-empty result
   aborts the migration.
6. Record the authority marker (a validated `schema_capabilities` singleton row
   and the mirrored `PRAGMA user_version`, §4.5), written **last** inside the
   same transaction.

Because the rebuilt table exposes no `file_path` identifier, a pre-R11 binary's
`SELECT`/`INSERT` naming `file_path` fails closed at statement preparation; this
is the mechanical half of the §4.5 guard.

`revalidate_critical_objects` (`src/db/migration/mod.rs:58-67`) gains a
`revalidate` for the new migration that reasserts the `display_path`,
`native_path`, and `native_path_state` columns, the partial unique index, and
the `schema_capabilities` marker, matching the pattern in migration 20.

### 4.3 Deterministic legacy backfill

The migration performs only filesystem-free, provable backfill:

- If `display_path` contains **no** U+FFFD: the display string is a faithful
  UTF-8 rendering, so encode the absolute native path from its bytes (Unix), its
  UTF-16 code units (Windows), or its UTF-8 bytes (other). Set
  `native_path_state = 2` (`legacy_verified_utf8`).
- If `display_path` contains **any** U+FFFD: the string may be a literal
  replacement character, one collapsed invalid byte, or many. Identity is **not
  provable**, so `native_path` stays `NULL` and
  `native_path_state = 3` (`quarantined_ambiguous`). No guess is made, and the
  migration never consults size, mtime, root, or file cardinality.

Why "no U+FFFD" is sufficient proof on Unix/Windows: `to_string_lossy` only
inserts U+FFFD when it encounters malformed native code units, so a stored
string without U+FFFD round-trips to exactly the bytes that produced it. The
converse (a stored U+FFFD) is exactly the ambiguous case and is quarantined even
if the file is valid UTF-8, because the same stored value is reachable from
distinct native paths.

### 4.4 Quarantine state machine

```text
                 migration
  (none) ────────────────────> authoritative (1) ── rename/scan ──> authoritative
              │  display has no U+FFFD (exact round-trip)
              │
              ├──────────────> legacy_verified_utf8 (2) ── exact key re-observed ──> authoritative (1)
              │                                          └─ key mismatch / not observed ──> quarantined (3)
              │
              └── display has U+FFFD ────> quarantined_ambiguous (3) ── exact retained key only ──> authoritative (1)
                                                                    └─ otherwise ────────────────> stays (3)
```

- **Quarantined rows are never deleted or silently re-identified.** Their id,
  ratings, play counts, history, and playlist bindings are retained. They are
  excluded from playback/tag-write/export authority and from scan dedup, and are
  presented as unavailable with a localized reason.
- **State 0 (`legacy_unresolved`) has no authority**, exactly like state 3, and
  exists only as the column default for a row written without a key. It is
  resolved, or stays keyless, by the same evidence rules; it never authorizes
  playback or a write on its own.
- **Adoption requires exact retained evidence, never a heuristic.** A
  quarantined (`U+FFFD`) row is rebound only through an exact native locator
  that was recorded independently of the lossy display string (§5.4). Size,
  mtime, root, display text, and cardinality are explicitly not evidence. The
  migration retains no such locator for ambiguous rows, so in practice state 3
  is terminal: observed files enroll as fresh authoritative rows while the
  quarantined row keeps its history.
- **`legacy_verified_utf8`** rows are provable (their display string
  round-trips byte-for-byte) and already carry an exact canonical `native_path`.
  They are promoted to state 1 on the first authoritative scan that re-observes
  a file whose canonical native key equals the row's encoded key; a key mismatch
  quarantines the row rather than guessing. Because their display text is an
  exact locator too, they are safe for both legacy and upgraded consumers.

### 4.5 Backward compatibility, version guard, and downgrade

The encoding is reversible by construction, but **backward compatibility is not
provided and is not claimed**. A database carrying native-path authority may
only be opened by a binary that implements this contract; running a pre-R11
binary against it is unsupported. Two independent mechanisms enforce that:

1. **Supported-version startup guard.** The migration records
   `native_path_authority_version = 1` in a validated singleton
   `schema_capabilities` row (mirrored to `PRAGMA user_version` for cheap
   pre-open detection), written last inside the migration transaction. On every
   open, the application reads the marker and:
   - refuses to open when the database's authority version exceeds the binary's
     compiled `MAX_SUPPORTED_NATIVE_PATH_AUTHORITY_VERSION`, with a closed
     `DatabaseAuthorityVersionUnsupported` diagnostic — this rejects an
     R11-aware but older reader/writer;
   - refuses to scan or write when the database declares authority but the
     binary lacks the §7/§9 fail-closed consumer baseline.
   A database whose marker is absent is pre-R11 and unaffected.
2. **Mechanical carrier removal.** The rebuilt `tracks` table has no `file_path`
   column (§4.2); display text lives only in `display_path`. A pre-guard binary
   that selects or writes `file_path` fails closed at statement preparation
   ("no such column") instead of regaining lossy authority. Successful SQL
   reads/inserts of the legacy column are therefore explicitly **not** evidence
   of compatibility: they cannot occur.

Validation requires an **old-binary / open-database rejection trace** test that
simulates a pre-R11 consumer (prepare `SELECT file_path FROM tracks`, and an
insert naming `file_path`) and asserts both fail closed, while `tracks` row ids,
ratings, play counts, history, and playlist references remain byte-identical
across the upgrade.

**Downgrade is separate from reopening.** Reopening the upgraded database with
an older binary is not a downgrade and is refused by the guard. The only
supported downgrade is the migration's `down()`, which restores the `file_path`
column name and refuses whenever any row would be left ambiguous under the old
schema — any non-`NULL` `native_path` whose decoded bytes differ from its
display text, or any state-0/3 row — matching the `drop_if_lossless` refusal
pattern of migration 20. `down()` is transactional and idempotent.

**Rollback / restart.** The table rebuild, the backfill, the index creation, and
the `schema_capabilities` marker write all execute in a single transaction. A
crash or power loss leaves either the fully pre-migration or the fully
post-migration schema; a restart re-runs idempotently. No partially activated
intermediate schema is observable to a reader.

## 5. Contract: scanner lookup and reconciliation

### 5.1 Keys

- The scanner computes the native key once per observed file
  (`NativePath::encode_absolute(path)`) and keys every set/map on that value:
  `on_disk_paths: HashSet<NativePathKey>`, `existing_by_path:
  HashMap<NativePathKey, &track::Model>`, rename destinations, and stale
  membership. No scanner decision uses `to_string_lossy` as a key.
- `ParsedTrack` carries the native key (and the lossy display string) so
  `apply_parsed_track_fields` writes both columns.
- An over-bound or unencodable path is skipped with a bounded diagnostic; it is
  never assigned a lossy key.

### 5.2 Enrollment and update

- Lookup is by `native_path`; insert sets both `native_path` (state 1) and the
  display `display_path`; update refreshes both.
- A file whose native key is absent creates a new row. Two distinct native keys
  never collide even when their display text is identical.
- The display string is recomputed from the current native path, so it is
  always consistent with the authoritative locator.

### 5.3 Stale removal

`should_remove_stale_track` tests membership by native key. A row is removed
only when its own native key is absent from a reconciliation-authoritative
scan; a different file that happens to render to the same display text can
never keep it alive or evict it.

### 5.4 Legacy adoption (exact evidence only, never a heuristic)

Adoption is the rebind of an existing row to an exact native locator. It is
permitted **only** from independently retained historical identity evidence: a
byte-exact native key recorded separately from the lossy display string — a
stored `native_path`, or an exact decode-then-re-encode match of an
already-encoded key. Size, mtime, display text, root, file cardinality, and
file content are **not** identity evidence and must never drive a rebind.

1. Only an exact encoded key that already round-trips for the row may be
   written into `native_path`; the canonicality check of §3.1 is the gate.
2. A scan re-observing that exact key promotes a `legacy_verified_utf8` row to
   state 1.
3. A `quarantined_ambiguous` (U+FFFD) row has **no** retained native bytes. The
   migration retains none, and a later scan cannot manufacture the missing
   evidence: the filesystem only shows what exists *now*. Therefore state 3 is
   terminal unless an exact native locator for that row was recorded
   independently before the lossy value was persisted. Observed files whose
   display text matches a quarantined row enroll as **fresh authoritative
   rows**; the quarantined row keeps its id, ratings, play counts, history, and
   playlist bindings, is presented as unavailable, and is never deleted or
   re-identified.
4. Playlist bindings are never reassigned from a quarantined row as part of
   adoption. A fresh row starts with no inherited history.

The previously published one-to-one size/mtime rule is **withdrawn**: copied
files commonly preserve both size and mtime, so a single surviving candidate
can be a different object entirely, and even an unchanged cardinality does not
establish which native name produced a historical lossy string (see the
mandatory trace in §8.4a). The acceptance's "preserve only where exact identity
is provable" is honored by refusing adoption where it is not provable, not by
raising the confidence of a guess.

### 5.5 Rename

- **File rename** retargets by native key: the source row is found by
  `native_path == encode(from)`, verified, and updated to `encode(to)`.
- **Directory rename** keys `from`/`to` by native scheme. The current explicit
  collision rejection (`engine.rs:2175-2190`) becomes unreachable for genuine
  native collisions because the keys are now distinct; it remains as a
  fail-closed guard and should be retained.
- **Root reauthorization** no longer needs to reject non-UTF-8 destinations
  (`engine.rs:1180-1182,1233`); retarget through the native codec and only
  refuse if the new native key cannot be encoded or exceeds the bound.

## 6. Contract: consumers

### 6.1 Playback resolution

`resolve_track` (`src/local/resolver.rs:334`) decodes `model.native_path` via
the codec and derives the `PathBuf` from the decoded native path; it never calls
`PathBuf::from(&model.display_path)`. A row with `native_path IS NULL` or state
0/3 returns a closed unavailable error (`LocalMediaResolutionError`), not a
guessed path. The existing retained-authority ordering (acquire root lease, open
the regular file under the retained handle, re-read the row and compare) is
preserved; the comparison becomes native-key equality.

### 6.2 Tag writes (coordinate with R1 / #248)

R1 owns exact tag-write authority. R11 contributes the locator:

- The write target must be derived from `track.id` → row → decoded
  `native_path` under retained root authority, not from a display URI.
- A quarantined or non-authoritative row refuses the write with a closed,
  localized conflict/unavailable result.
- The native path must carry the same byte-exact identity into R1's
  content-revision and replacement checks, so "the selected track" is provably
  the same native object through preview, copy, and commit.

The URI round-trip currently used for local targets
(`src/ui/window.rs:4622-4624`, `src/ui/context_menu.rs:1349-1354`) is not a
sound authority carrier for a non-UTF-8 name and is retired for local writes in
favor of the id-keyed lookup.

### 6.3 Import / export

- **XSPF export** writes the location from the decoded native path using a
  canonical percent-encoded form of the native bytes (XSPF `<location>` is XML
  text, so raw invalid bytes cannot appear). The exact mapping is defined with
  the codec and covered by tests. Until that mapping ships (R11d), export must
  **fail closed**: it omits or diagnoses non-authoritative rows rather than
  emitting a lossy `display_path` location. This is part of the §7 baseline and
  must land before schema/scanner activation, so an intermediate build can never
  export a false locator for a newly enrolled row.
- **XSPF import** decodes the percent-encoded location losslessly back to a
  native path and matches by native key first; the metadata/duration fingerprint
  fallback (`ImportedTrackMatchIndex`, `playlist_io.rs:591-700`) is unchanged
  and remains lower precedence.
- `playlist_entries.match_file_path` stays display/fingerprint text but must
  never be the sole authority; local matching prefers
  `playlist_entries.local_track_id` and the native row.
- **Rhythmbox import/migration** currently refuse non-UTF-8 locations
  (`rhythmbox_import.rs:1534`). Under this contract a non-UTF-8 Rhythmbox
  location becomes representable; the exact scope (accept losslessly vs. keep
  refusing and diagnose) is a follow-up decision recorded in §10. Until then
  the refusal stands and is documented, never silently lossy.

### 6.4 Display and search

Presentation continues to use `display_path`. Search may match display text; any
result that leads to an action resolves through the row id and native key.

## 7. Safe refusal and diagnostics

This section defines the **fail-closed authority baseline**. It is the first
implementation slice (R11a) and a mandatory prerequisite for schema/scanner
activation: no authority consumer may act on a row whose exact native identity
it cannot establish. Until the representation is authoritative, and permanently
for unrepresentable input, the boundary is:

- **Replacement-character refusal.** A stored path containing U+FFFD cannot
  prove identity. Playback, tag writes, stale removal/reassignment, and export
  all refuse it with a closed unavailable reason until an exact native key
  proves the row. This replaces "silently storing false playback authority."
- **Collision refusal.** On scan/watcher enrollment, if a native path's lossy
  display text already belongs to a row with a different native key, refuse to
  update that row, emit a bounded diagnostic, and (once the schema lands) create
  a distinct row instead. Never overwrite authority.
- **Export refusal.** XSPF export never emits a lossy `display_path` as a
  location. Non-authoritative rows are omitted or diagnosed; authoritative rows
  are emitted only from a decoded native key (§6.3). Import accepts a location
  as authority only when it decodes to an exact native key.
- **Closed, redacted diagnostics.** Diagnostics name a category, not raw native
  bytes or absolute paths, consistent with the path-free diagnostics rule in
  `docs/architecture/source-lifecycle.md`. Categories: unrepresentable path,
  over-bound encoding, malformed scheme, duplicate display text, ambiguous
  legacy identity.
- **Localized user-visible reasons** for quarantined/unavailable rows, added to
  every supported locale catalog, mirroring the folder-root unavailable-message
  pattern.

This baseline only removes false authority and changes no stored data, so it is
safe to land alone, and it must precede any native-row enrollment.

## 8. Test and fixture contract

All native-name fixtures use `${TMPDIR:-/var/tmp}`. The audio payload is a small
generated/committed WAV (the existing minimal-WAV generator in
`src/local/tag_parser.rs` tests, or a committed `tests/fixtures/audio` payload);
only the filename bytes vary.

Required tests:

1. **Distinct invalid bytes (Linux/unix).** Create `a\xff.flac` and
   `a\xfe.flac` with `OsString::from_vec`; assert two distinct native keys, two
   distinct rows, and that each resolves to its own file. This is the core
   reproduction and must be `#[cfg(unix)]`.
2. **Literal replacement-character collision.** Create a valid UTF-8 file named
   with a literal U+FFFD and an invalid-byte file whose lossy rendering is
   identical; assert distinct native keys and no unique-constraint failure.
3. **Backfill classification.** Construct rows with and without U+FFFD and
   assert `legacy_verified_utf8` vs `quarantined_ambiguous`.
4. **Adoption requires exact evidence; heuristics never adopt.**
   - **4a (mandatory non-adoption trace: missing original / single stranger).**
     Seed an ambiguous legacy row whose display is `a\u{FFFD}.flac`; the
     original file is absent and exactly one different file — `a\u{FE}.flac`,
     or a literal replacement-character file — is observed at the same root
     with identical size and mtime. Assert the row stays
     `quarantined_ambiguous`, its history/rating/playlist bindings are
     unchanged, no playback/write/export authority is granted, and a fresh
     authoritative row is enrolled for the observed file.
   - **4b (exact-key promotion only).** A `legacy_verified_utf8` row is promoted
     to authoritative only when the same canonical native key is re-observed; a
     display-matching but key-different file never promotes, and a state-3 row
     with no retained exact locator never adopts.
5. **Playback resolution.** `resolve_track` on a non-UTF-8 row opens the correct
   native file; a quarantined row returns the closed unavailable error.
6. **Stale removal / membership.** A surviving different file with the same
   display text does not keep a dead row alive; a live non-UTF-8 row is not
   removed.
7. **Rename.** File and directory rename over non-UTF-8 names retarget rows by
   native key without the current collision rejection.
8. **Unicode / normalization.** Supported-platform cases where NFC and NFD (or
   case/symlink variants) are distinct native names must stay distinct; do not
   normalize identity. Assert the display text may differ or coincide without
   changing the key.
9. **Codec unit tests.** Round-trip, canonicality rejection (uppercase hex,
   odd length, NUL, unknown scheme), and the size bound, mirroring
   `identity.rs` tests.
10. **Import/export round-trip.** XSPF export/import of a non-UTF-8 location
    preserves the native key.
11. **Old-binary / open-database rejection trace (F3).** Simulate a pre-R11
    consumer: a prepared `SELECT file_path FROM tracks` and an insert naming
    `file_path` both fail closed ("no such column"); the
    `native_path_authority_version` startup guard refuses a binary that does not
    support the database's authority version; `down()` refuses while any row
    would be ambiguous under the old schema and otherwise restores the
    `file_path` column name. Assert
    row ids, ratings, play counts, history, and playlist references are
    byte-identical across the upgrade.
12. **Intermediate-version integration matrix (F2).** For every independently
    landable slice state (R11a baseline only; R11a+R11b; R11b without R11c;
    R11c without R11d), run the resolver, tag-write, export, and stale-removal
    paths against (a) an invalid-byte row whose display contains U+FFFD,
    (b) a literal replacement-character row, and (c) an authoritative native
    row before its consumer has shipped; assert a closed refusal with no
    wrong-file open, no write, and no lossy export.

Physical-platform validation remains separate from automated checks: APFS and
Windows reject invalid-byte filenames, so the end-to-end scan/reproduction is a
Linux check plus documented platform limits, as the issue already states. CI
must gate the non-UTF-8 tests on `cfg(unix)` and skip cleanly elsewhere.

## 9. Rollout plan (follow-up implementation beads)

This design authorizes five bounded slices; none is this bead. Ordering is a
safety contract, not a preference: no slice may make a consumer *more
permissive* than the fail-closed baseline, and schema/scanner activation must
not expose a newly-lossless row to a consumer that cannot prove it.

1. **R11a — fail-closed authority baseline (must land first).** Every authority
   consumer stops trusting display text, per §7: the resolver refuses a
   U+FFFD-bearing display path with a closed unavailable reason instead of
   `PathBuf::from(display_path)`; the tag-write target refuses it; stale and
   watcher removal never use display text as authority for a U+FFFD key (no
   wrong delete/reassign); XSPF export omits/diagnoses U+FFFD locations instead
   of emitting a false locator and import refuses unprovable locations; the UI
   tag-write URI path refuses. This slice only removes false authority, so it is
   safe standing alone and changes no stored data.
2. **R11b — codec + schema + scanner keys (activation).** Shared
   absolute-native-path codec; migration `000021` (rename to `display_path`,
   native columns, table rebuild, partial unique index, backfill,
   `schema_capabilities` marker); scanner/stale/rename keying by native path.
   Activation is gated on R11a: the binary refuses to write the marker or enroll
   native rows unless the fail-closed baseline is compiled in (§4.5). New rows
   (including invalid-byte paths) enroll with authority; existing consumers
   remain fail-closed for them until R11c/R11d.
3. **R11c — playback + tag-write authority.** Resolver decodes `native_path`;
   tag-write target id-keyed; coordinate the R1 content-revision boundary.
4. **R11d — import/export + Rhythmbox.** Lossless XSPF location mapping; export
   switches from baseline omission to lossless emission only for authoritative
   rows; decide and document the Rhythmbox non-UTF-8 scope.
5. **R11e — legacy cleanup.** Remove any remaining display-as-authority fallback
   once scans prove the native keys; keep `display_path` display-only.

Ordering rationale: R11a closes the active harm and establishes the invariant
"no consumer acts on a row it cannot prove"; R11b may then safely introduce
native rows because every consumer already fails closed for anything it cannot
prove. Consumers gain lossless capability only after the baseline, never before.
The §7 baseline may be split into independently landable per-consumer commits,
but no schema/scanner activation may precede the complete baseline.

**Intermediate-version integration tests (required).** For each independently
landable state (R11a only; R11a+R11b; R11b without R11c; R11c without R11d), run
the resolver, tag-write, export, and stale-removal paths against (a) an
invalid-byte row whose display contains U+FFFD, (b) a literal
replacement-character row, and (c) an authoritative native row before its
consumer has shipped, and assert a closed refusal. This proves each slice is
safe alone, not only the final architecture.

## 10. Open questions and coordination

- **Root key lossiness.** `library_roots.path` remains a lossy `String` PK.
  This contract refuses a non-UTF-8 root and keeps roots valid-UTF-8. Whether to
  later give roots a native key is a root/scanner-owner decision, explicitly out
  of scope here.
- **Rhythmbox non-UTF-8.** Accept losslessly or keep the documented refusal —
  decide during R11d.
- **R1 / #248 interface.** The exact field/function through which the decoded
  native path and the content revision cross into tag-write authority is owned
  by R1; R11 supplies the locator and will not duplicate R1's revision logic.
- **XSPF percent-encoding details.** Define one canonical byte-level
  percent-encoding and reject non-canonical spellings, matching the codec's
  canonicality discipline.
- **Two colliding rows in the UI.** Confirm the localized presentation for
  identical display text that resolves to distinct files.
- **Migration table rebuild on large libraries.** The rebuild copies every
  `tracks` row; measure against the library-size budget record and split
  backfill from the rebuild if needed. The marker write stays in the same
  transaction as the rebuild so no half-activated schema is observable.
- **Release gating for the compatibility break.** The `file_path` →
  `display_path` rename plus the authority marker make an upgraded database
  unusable by pre-R11 binaries. The release that ships R11b must state this,
  name the supported `down()` downgrade, and keep non-Dependabot auto-merge off
  as the operator requires.

## 11. Alternatives considered

- **Keep lossy strings, disambiguate display text with a suffix.** Rejected:
  it fabricates display distinctions, leaves playback/tag-write authority lossy,
  and does not preserve identity.
- **Replace `TrackId` with the encoded path.** Rejected: local track identity is
  already the stable DB UUID; changing it would break queues, history, ratings,
  and playlist references for no benefit. R11 changes the locator, not the
  identity.
- **Refuse all non-UTF-8 paths (boundary only).** Rejected as the sole answer:
  it is honest but needlessly drops a class of the user's media. It is retained
  as the permanent boundary for unrepresentable input and as the interim rule.
- **Root-relative native keys only.** Considered; absolute native keys preserve
  current root semantics with less churn. Root-relative identity can be a later
  refinement if root relocation needs it.

## 12. Acceptance mapping

| Acceptance item | Where satisfied |
|---|---|
| Versioned reversible representation or explicit boundary | §1, §3, §7 |
| Display text separated from authoritative identity (enforced by the column rename) | §1, §3.4, §4.1, §4.2, §5, §6.4 |
| Preserve IDs/history/ratings/playlists only where exact identity is provable | §2.1, §4.2, §4.3, §5.4, §8.4a |
| Quarantine ambiguous legacy rows, no guessing (heuristics withdrawn) | §4.3, §4.4, §5.4, §8.4a |
| Scanner lookup/reconciliation, migration, playback, tag writes, import/export | §4, §5, §6 |
| Linux fixtures: invalid bytes, literal replacement collisions, Unicode/normalization, rename | §8 |
| Safe rejection/diagnostics until lossless support | §7 |
| No lossy consumer exposed to newly encoded rows (fail-closed-first) | §7, §9, §8.12 |
| Older-binary / version-guard containment (no false authority re-enabled) | §4.2, §4.5, §8.11 |

## 13. Corrective revision mapping (independent review of `16140be0`)

This revision answers the three P1 findings in
`refinery-20260917-tr-ldhwt/corrective-instructions.md`:

| Finding | Required correction | Where changed | Validation added |
|---|---|---|---|
| **F1** — single-candidate size/mtime legacy adoption guesses identity | Adoption must rest on independently retained exact evidence, or the row stays quarantined and the observed file enrolls fresh; align the state machine, acceptance mapping, and fixture 4; add the missing-original/single-stranger trace | Withdrew the §5.4 one-to-one size/mtime rule; made state 3 terminal absent an exact retained key; updated the §4.4 machine, §12 mapping, and §8.4 fixtures | §8.4a mandatory non-adoption trace; §8.4b exact-key-only promotion |
| **F2** — staged rollout exposes lossy consumers to newly lossless rows | Every authority consumer must fail closed first, or activation and consumer switches must be atomic; specify export handling and rollback/restart; add intermediate-version integration tests | Split out the §7 fail-closed baseline as R11a and gated schema/scanner activation (R11b) on it; made export refusal part of the baseline; specified the single-transaction marker write and restart behavior; added the §9 intermediate matrix | §8.12 per-slice refusal matrix; §8.6 stale-removal refusal; §9 ordering |
| **F3** — older-binary compatibility re-enables false authority | A real supported-version/startup guard or a demonstrable compatibility barrier; remove the SQL-compatibility claim; add an old-binary/open-database rejection trace; separate downgrade from reopening | Rewrote §4.5: `schema_capabilities`/`user_version` guard, `file_path` → `display_path` carrier removal, no compatibility claim, `down()` only as downgrade, transactional rollback | §8.11 old-binary rejection trace and intact-history assertion |

Nothing in this revision expands product behavior; every change makes the
contract stricter. The mechanical check results recorded at `16140be0` remain
historical; the next independent re-review should evaluate this corrected head.

## Appendix A — Lossy conversion inventory (`src/local/`)

Grounded at the design commit; implementation must convert each to the native
key. The `file_path` column is renamed `display_path` by this contract; the
entries below name the legacy identifier as it exists at the design commit.

| Location | Current lossy use | New authority |
|---|---|---|
| `tag_parser.rs:169` | `ParsedTrack.file_path` | carry native key + display |
| `engine.rs:3892-3893` | scan dedup + `on_disk_paths` | native-key set/map |
| `engine.rs:2114,2184,2237` | dir-rename observed/destination keys | native-key set |
| `engine.rs:2318-2323` | stale membership | native-key test |
| `engine.rs:4759` | watcher removal lookup | native-key lookup |
| `engine.rs:5902,5936,6020` | dir-rename surplus / removal events | native key |
| `engine.rs:6400-6444` | file rename retarget | native key |
| `engine.rs:6504-6573` | dir rename prefix/join | native key |
| `engine.rs:1180-1182,1233` | root reauthorization non-UTF-8 refusal | native codec |
| `engine.rs:2753,2868,2953,2989,3647,3705,4343,4356` | `library_roots.path` keys | unchanged (root boundary) |
| `resolver.rs:353` | playback `PathBuf::from(file_path)` | decode `native_path` |
| `playlist_io.rs:89,732,754-766` | XSPF location encode/decode | lossless mapping |
| `playlist_io.rs:591-700` | import match index | native key, then fingerprint |
| `rhythmbox_import.rs:1534` | non-UTF-8 refusal | R11d decision |
| `rhythmbox_migration.rs:1026-1264` | path string matching | native key |
| `ui/window.rs:4622-4624`, `ui/context_menu.rs:1349-1354` | display-URI tag target | id-keyed lookup |
