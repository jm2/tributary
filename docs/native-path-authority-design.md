# Native local-path authority design (R11 / #258)

Status: design contract, no implementation in this bead. This document is the
deliverable the R11 acceptance calls for: a versioned reversible native-path
representation plus an explicit unsupported-input boundary, for review before
any behavior changes. It authorizes follow-up implementation beads; it does not
ship them.

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
   (the scheme tag is the version). The existing `tracks.file_path` string
   becomes non-authoritative display text and loses its UNIQUE constraint.
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
replacement character U+FFFD must not be used as playback or tag-write
authority.

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
fall back to `file_path`.

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

- `tracks.file_path` becomes **display text**: a lossy, possibly duplicated
  human-readable rendering used for presentation, search, and fallback labels.
  It is never used to open a file, to key uniqueness, or to prove identity.
- `tracks.native_path` is the **authoritative locator and uniqueness key**.
- The UI may show the lossy rendering (two colliding files legitimately render
  identically; honesty beats fake disambiguation). Any code that needs to
  *act* on the file must go through `native_path` decoded under retained root
  authority.

## 4. Contract: schema and migration

Follow the migration conventions in `src/db/migration/` (`m<YYYYMMDD>_<6-digit>_<name>.rs`,
registered in `mod.rs`). The next free slot is `000021` after
`m20260903_000020_lastfm_policy.rs` (`src/db/migration/mod.rs:24,51`).

### 4.1 New columns

- `tracks.native_path TEXT NULL` — the encoded authoritative locator. `NULL`
  means "not yet proven" (legacy quarantine) and nothing else.
- `tracks.native_path_state INTEGER NOT NULL DEFAULT 0` — a closed enum:

  | Value | Name | Meaning |
  |---|---|---|
  | 0 | `legacy_unresolved` | Column default; a keyless row with no proof; no authority |
  | 1 | `authoritative` | `native_path` is encoded, canonical, and current |
  | 2 | `legacy_verified_utf8` | Backfilled from a U+FFFD-free display string |
  | 3 | `quarantined_ambiguous` | Decode failed, collision, or unprovable legacy |

  Only state 1 (and, transitionally, 2 before its first authoritative rescan)
  grants playback/tag-write/export authority.

- `tracks.native_path_scheme TEXT NULL` — optional denormalized scheme for
  cheap filtering/validation. It must equal the decoded scheme of
  `native_path`; `NULL` when `native_path` is `NULL`.

### 4.2 Uniqueness moves to the native key

`tracks.file_path` must lose its UNIQUE constraint: two distinct native paths
may legitimately render to the same display text, and both must be storable and
visible. Add a unique index on `native_path` (partial, `WHERE native_path IS
NOT NULL`) and keep `file_path` as a plain indexed display column.

SQLite cannot drop a UNIQUE constraint in place, so the migration performs a
table rebuild following the existing raw-SQL migration style used by migration
13 and 20:

1. `PRAGMA foreign_keys` is off for the rebuild window (or the table is rebuilt
   with `legacy_alter_table` semantics), because
   `playlist_entries.local_track_id` references `tracks(id)`.
2. Create `tracks_new` with the full current column set plus the three new
   columns; `id` stays the primary key, `file_path` is no longer unique.
3. Copy every row, computing the new columns in the migration's Rust loop (hex
   encoding is not expressed in plain SQL): a U+FFFD-free `file_path` gets its
   encoded `native_path` and state 2; any other `file_path` gets `NULL` and
   state 3. Row ids, ratings, play counts, `last_played_at_ms`, `date_added`,
   `date_modified`, and `file_size_bytes` are copied verbatim.
4. Drop `tracks`, rename `tracks_new` to `tracks`, recreate the artist/album/
   genre indexes and the new partial unique `native_path` index.
5. Re-enable foreign keys and run `PRAGMA foreign_key_check`; a non-empty result
   aborts the migration.

`revalidate_critical_objects` (`src/db/migration/mod.rs:58-67`) gains a
`revalidate` for the new migration that reasserts the `native_path` index and
the `native_path`/`native_path_state` columns exist, matching the pattern in
migration 20.

### 4.3 Deterministic legacy backfill

The migration performs only filesystem-free, provable backfill:

- If `file_path` contains **no** U+FFFD: the display string is a faithful UTF-8
  rendering, so encode the absolute native path from its bytes (Unix), its
  UTF-16 code units (Windows), or its UTF-8 bytes (other). Set
  `native_path_state = 2` (`legacy_verified_utf8`).
- If `file_path` contains **any** U+FFFD: the string may be a literal
  replacement character, one collapsed invalid byte, or many. Identity is **not
  provable**, so `native_path` stays `NULL` and
  `native_path_state = 3` (`quarantined_ambiguous`). No guess is made.

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
              │  U+FFFD-free
              │
              ├──────────────> legacy_verified_utf8 (2) ── rescan ──> authoritative
              │                                                  └─ mismatch ─> quarantined
              │
              └── U+FFFD ────> quarantined_ambiguous (3) ── unique proof ─> authoritative
                                                       └─ still ambiguous ─> stays (3)
```

- **Quarantined rows are never deleted or silently re-identified.** Their id,
  ratings, play counts, and playlist bindings are retained. They are excluded
  from playback/tag-write/export authority and from scan dedup, and are
  presented as unavailable with a localized reason.
- **State 0 (`legacy_unresolved`) has no authority**, exactly like state 3, and
  exists only as the column default for a row written without a key (for
  example by an older binary after the schema migration). A scan resolves it by
  the same proof rules; it never authorizes playback or a write on its own.
- **Adoption is scan-time and proof-only.** A quarantined row is rebound to
  `authoritative` only when an authoritative scan can prove a one-to-one match
  (see §5.4). Any other outcome leaves it quarantined.
- **`legacy_verified_utf8`** rows behave like authoritative rows for lookup but
  are promoted to state 1 on the first authoritative scan that re-observes the
  exact native key.

### 4.5 Reversibility / downgrade

The encoding is reversible by construction. The schema change is additive from
the application's perspective: `file_path` still holds valid display text for
every row, so an older binary can still read the table (it will ignore the new
columns and the dropped uniqueness, and its own inserts remain valid because
the columns are nullable/defaulted). `down` is allowed only when no row has a
non-`NULL` `native_path` that the old schema could not represent; otherwise it
refuses, matching the `drop_if_lossless` refusal pattern in migration 20.

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
  display `file_path`; update refreshes both.
- A file whose native key is absent creates a new row. Two distinct native keys
  never collide even when their display text is identical.
- The display string is recomputed from the current native path, so it is
  always consistent with the authoritative locator.

### 5.3 Stale removal

`should_remove_stale_track` tests membership by native key. A row is removed
only when its own native key is absent from a reconciliation-authoritative
scan; a different file that happens to render to the same display text can
never keep it alive or evict it.

### 5.4 Legacy adoption (proof-only)

When an authoritative scan observes native paths, it attempts adoption for
quarantined rows only under a strict one-to-one rule:

1. Group observed native keys by their lossy display string.
2. Group quarantined rows by their stored `file_path`.
3. A rebind is allowed only when a display group contains **exactly one**
   observed native key **and exactly one** quarantined row, and the observed
   file's size and mtime match that row's stored `file_size_bytes` and
   `date_modified`, and the row's old path resolved under the same root.
4. Rebind sets `native_path` to the observed key and state to 1. The row id,
   history, and playlist bindings are untouched.

If a display group contains two or more observed native keys, or two or more
quarantined rows, no pairing is attempted: all quarantined rows in that group
stay quarantined and fresh authoritative rows are created for the observed
files. This is the exact trade the acceptance calls for: preserve history where
identity is provable, quarantine (do not guess) otherwise.

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
`PathBuf::from(&model.file_path)`. A row with `native_path IS NULL` or state 3
returns a closed unavailable error (`LocalMediaResolutionError`), not a guessed
path. The existing retained-authority ordering (acquire root lease, open the
regular file under the retained handle, re-read the row and compare) is
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
  the codec and covered by tests.
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

Presentation continues to use `file_path`. Search may match display text; any
result that leads to an action resolves through the row id and native key.

## 7. Safe refusal and diagnostics

Until the representation is authoritative, and permanently for unrepresentable
input, the boundary is:

- **Collision refusal.** On scan/watcher enrollment, if a native path's lossy
  display text already belongs to a row with a different native key, refuse to
  update that row, emit a bounded diagnostic, and (once the schema lands) create
  a distinct row instead. Never overwrite authority.
- **Replacement-character refusal.** A stored path containing U+FFFD cannot
  prove identity. Refuse playback and tag writes for it with a closed
  unavailable reason until a scan proves the native key. This replaces "silently
  storing false playback authority."
- **Closed, redacted diagnostics.** Diagnostics name a category, not raw native
  bytes or absolute paths, consistent with the path-free diagnostics rule in
  `docs/architecture/source-lifecycle.md`. Categories: unrepresentable path,
  over-bound encoding, malformed scheme, duplicate display text, ambiguous
  legacy identity.
- **Localized user-visible reasons** for quarantined/unavailable rows, added to
  every supported locale catalog, mirroring the folder-root unavailable-message
  pattern.

This section is the minimal implementation slice that closes the active harm
before the full representation lands.

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
4. **Proof-only adoption.** One ambiguous row + one observed native file with
   matching size/mtime adopts; two observed files collapsing to one display
   string leaves every ambiguous row quarantined and creates fresh rows.
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

Physical-platform validation remains separate from automated checks: APFS and
Windows reject invalid-byte filenames, so the end-to-end scan/reproduction is a
Linux check plus documented platform limits, as the issue already states. CI
must gate the non-UTF-8 tests on `cfg(unix)` and skip cleanly elsewhere.

## 9. Rollout plan (follow-up implementation beads)

This design authorizes four bounded slices; none is this bead:

1. **R11a — codec + schema + scanner keys.** Shared absolute-native-path codec,
   migration `000021` (columns, table rebuild, partial unique index, backfill),
   scanner/stale/rename keying. This includes the §7 interim collision
   refusal where it can land independently.
2. **R11b — playback + tag-write authority.** Decode in resolver; id-keyed
   tag-write target; coordinate the R1 content-revision boundary.
3. **R11c — import/export + Rhythmbox.** Lossless XSPF location mapping; decide
   and document the Rhythmbox non-UTF-8 scope.
4. **R11d — legacy cleanup.** Remove any remaining `file_path`-as-authority
   fallback once scans prove the native keys; keep `file_path` display-only.

Ordering rationale: the schema and scanner keys must land before consumers can
trust `native_path`; the interim boundary in §7 closes the active harm earliest.

## 10. Open questions and coordination

- **Root key lossiness.** `library_roots.path` remains a lossy `String` PK.
  This contract refuses a non-UTF-8 root and keeps roots valid-UTF-8. Whether to
  later give roots a native key is a root/scanner-owner decision, explicitly out
  of scope here.
- **Rhythmbox non-UTF-8.** Accept losslessly or keep the documented refusal —
  decide during R11c.
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
  backfill from the rebuild if needed.

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
| Display text separated from authoritative identity | §1, §3.4, §5, §6.4 |
| Preserve IDs/history/ratings/playlists only where provable | §2.1, §4.2, §4.3, §5.4 |
| Quarantine ambiguous legacy rows, no guessing | §4.3, §4.4, §5.4 |
| Scanner lookup/reconciliation, migration, playback, tag writes, import/export | §4, §5, §6 |
| Linux fixtures: invalid bytes, literal replacement collisions, Unicode/normalization, rename | §8 |
| Safe rejection/diagnostics until lossless support | §7 |

## Appendix A — Lossy conversion inventory (`src/local/`)

Grounded at the design commit; implementation must convert each to the native
key.

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
| `rhythmbox_import.rs:1534` | non-UTF-8 refusal | R11c decision |
| `rhythmbox_migration.rs:1026-1264` | path string matching | native key |
| `ui/window.rs:4622-4624`, `ui/context_menu.rs:1349-1354` | display-URI tag target | id-keyed lookup |
