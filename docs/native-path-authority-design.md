# Native local-path authority design (R11 / #258)

Status: design contract, no implementation in this bead. This document is the
deliverable the R11 acceptance calls for: a versioned reversible native-path
representation plus an explicit unsupported-input boundary, for review before
any behavior changes. It authorizes follow-up implementation beads; it does not
ship them.

Revision 4 (this head) continues the corrective chain over the independently
rejected heads `16140be0d03d17c1f299cf7690adea6648722160` (Revision 2) and
`bc49dbe514334e94c081e3156ab97a12df11f066` (Revision 3). Revision 2 withdrew the size/mtime
legacy-adoption rule (F1), made the rollout fail-closed-first so no authority
consumer is ever exposed to a row it cannot prove (F2), and removed the
older-binary compatibility claim in favor of an enforced version guard plus a
mechanical carrier removal (F3). Revision 3 additionally corrects the migration
rebuild sequence: foreign-key suppression is a connection-level prelude before
the transaction begins, restoration is verified on every exit path, and
`legacy_alter_table` is banned as a substitute (the migration reference-loss
finding). Revision 4 answers the five round-2 review findings verified at
`6218ec03` (§13.2): the permanent root UTF-8 boundary is restored for root
reauthorization (R2-F1), the migration preservation gate compares exact
entry-to-track binding pairs (R2-F2), the §4.4 diagram matches the backfill
states the migration actually writes (R2-F3), the older-binary containment
claim is narrowed to what its two mechanisms actually deliver (R2-F4), and
XSPF export fails closed with an `Err` instead of omitting rows (R2-F5).
§13 is the finding-to-revision map. No behavior changes: every
revision makes the contract stricter, not more permissive.

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
   it removes the legacy authority carrier so that pre-R11 access to that
   authority column fails closed instead of silently regaining lossy authority
   over the upgraded database (§4.5 states the exact scope of that containment:
   statements naming the removed column, plus the contract-aware version
   guard — not a blanket executable rejection of pre-R11 binaries).
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
  | --- | --- | --- |
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
13 and 20. `playlist_entries.local_track_id` references `tracks(id)` with
`ON DELETE SET NULL` (`fk_entry_local_track`, §2.1), so the drop/rename window
must run with foreign-key enforcement suppressed — and the suppression is a
**connection-level prelude, never a statement inside the transaction**:

- SQLite silently ignores a `PRAGMA foreign_keys` change issued inside a
  transaction, so an in-transaction toggle leaves enforcement on and the
  `DROP TABLE tracks` fires `ON DELETE SET NULL` against
  `playlist_entries.local_track_id`, silently detaching every playlist entry
  from its track.
- `PRAGMA legacy_alter_table` is **not** a substitute and must not be used. It
  changes rename/linking behavior; it does not disable `ON DELETE` actions.
  With `foreign_keys=ON, legacy_alter_table=ON`, copying `tracks_new` then
  dropping/renaming still nulls existing playlist references, and
  `PRAGMA foreign_key_check` cannot see the damage because a NULL binding is
  legal.

The executable sequence on the single dedicated migration connection is:

0. **Connection prelude (before `BEGIN`).** Execute
   `PRAGMA foreign_keys = OFF` on the migration connection, then read
   `PRAGMA foreign_keys` back and require the result to be `0`; if the
   connection reports anything else, abort closed before any transaction or
   write. Record the prior value so it can be restored on every exit path.
   While the pragma is off at connection level, `ON DELETE` actions do not
   fire, which is what makes the drop/rename window safe. `playlist_entries`
   itself is never written by this migration.
1. `BEGIN` a single transaction for everything below.
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
5. **In-transaction preservation gate (before `COMMIT`).** Snapshot, before
   the drop, the exact set of populated playlist bindings — every
   `(playlist_entries.id, local_track_id)` pair with a non-NULL
   `local_track_id` — and require, against the rebuilt table: the same
   `tracks` row count with an identical id set; the same `playlist_entries`
   row count; and **pair-set equality** of that binding snapshot — each entry
   id must still be bound to exactly the track id it was bound to before, so
   no binding may change in any way: none may go from non-NULL to NULL, none
   may be rebound to a different track id, and swapping two entries' track ids
   fails the gate even though a swap preserves both the row count and the
   multiset of bound track ids. Then require an empty
   `PRAGMA foreign_key_check`. `foreign_key_check` alone is explicitly
   insufficient — `SET NULL` damage is invisible to it because NULL is legal —
   and count-plus-track-id-set comparisons are explicitly insufficient because
   they cannot detect swapped or otherwise rebound entries — so the exact
   pair-set comparison is mandatory. Any mismatch aborts the transaction and
   rolls everything back.
6. Record the authority marker (a validated `schema_capabilities` singleton row
   and the mirrored `PRAGMA user_version`, §4.5), written **last** inside the
   same transaction.
7. `COMMIT`.
8. **Connection epilogue (after the transaction ends, on every exit path).**
   Restore `PRAGMA foreign_keys` to its recorded prior value (normally `ON`)
   and read it back to verify the restoration took effect; run a final
   `PRAGMA foreign_key_check` as a belt-and-braces assertion. The restoration
   is wired so it also runs on the error and panic paths (a Rust drop guard),
   never only on the happy path.

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
              migration backfill (terminates in state 2 or 3;
                                  never writes state 1)
  (none) ── display has no U+FFFD ──> legacy_verified_utf8 (2)
          │                             ├─ exact key re-observed ──> authoritative (1)
          │                             └─ key mismatch / not observed ──> quarantined (3)
          │
          └── display has U+FFFD ───────────> quarantined_ambiguous (3)
                                                ├─ exact retained key only ──> authoritative (1)
                                                └─ otherwise ────────────────> stays (3)

  new post-migration enrollment (scan/watcher insert) ──> authoritative (1)
  authoritative (1) ── rename/scan re-observation ──> authoritative (1)
```

The migration itself never writes state 1: §4.2 step 3 and §4.3 terminate the
backfill in state 2 for a U+FFFD-free display string and state 3 for a
U+FFFD-bearing one. `authoritative` (1) is entered only by a later exact-key
re-observation of a state-2 row, by an exact retained key for a state-3 row
(§5.4), or by a new post-migration enrollment; there is no direct
migration→1 transition.

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
binary against it is unsupported. Two mechanisms contain the exposure — and
their scope must be stated exactly, because neither mechanically rejects an
arbitrary pre-R11 binary at open:

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

   A database whose marker is absent is pre-R11 and unaffected. The marker is
   read by contract-aware binaries only: a pre-R11 binary that knows nothing
   of `schema_capabilities` or `PRAGMA user_version` is not stopped by this
   guard.
2. **Mechanical carrier removal.** The rebuilt `tracks` table has no `file_path`
   column (§4.2); display text lives only in `display_path`. A pre-guard binary
   that selects or writes `file_path` fails closed at statement preparation
   ("no such column") instead of regaining lossy authority. Successful SQL
   reads/inserts of the legacy column are therefore explicitly **not** evidence
   of compatibility: they cannot occur. The barrier is scoped to statements
   that name the removed column: a statement that does not name `file_path` —
   `SELECT *`, or a read of the surviving columns (ids, ratings, play counts,
   history) — prepares and executes against the rebuilt table. An arbitrary
   pre-R11 binary is therefore unsupported-but-not-mechanically-rejected unless
   the statement it runs names `file_path`; no executable barrier in this
   design rejects every pre-R11 read or write.

Validation requires an **old-binary / open-database rejection trace** test that
simulates a pre-R11 consumer (prepare `SELECT file_path FROM tracks`, and an
insert naming `file_path`) and asserts both fail closed, while `tracks` row ids,
ratings, play counts, history, and playlist references remain byte-identical
across the upgrade.

**Downgrade is separate from reopening.** Reopening the upgraded database with
an older binary is not a downgrade, and reopening is not uniformly refused: an
older **contract-aware** binary — one that reads `schema_capabilities` /
`PRAGMA user_version` — is refused by the supported-version startup guard when
the database's authority version exceeds that binary's compiled maximum, while
an arbitrary pre-R11 binary is not stopped by that guard at all; it is
contained only by the statement-preparation failure when the statements it runs
name the removed `file_path` column, and is otherwise
unsupported-but-not-mechanically-rejected, exactly as scoped in items 1–2
above. The only
supported downgrade is the migration's `down()`, which restores the `file_path`
column name and refuses whenever any row would be left ambiguous under the old
schema — any non-`NULL` `native_path` whose decoded bytes differ from its
display text, or any state-0/3 row — matching the `drop_if_lossless` refusal
pattern of migration 20. `down()` is transactional and idempotent, and it must
also revert the authority markers: `up()` writes the `schema_capabilities`
authority singleton row and the mirrored `PRAGMA user_version` **last** inside
its transaction (§4.2 step 6), so a successful `down()` deletes that singleton
row and resets the mirrored `PRAGMA user_version` to its pre-R11 value in the
same transaction as the table rebuild. A downgraded database therefore
presents as pre-R11 — authority marker absent — to every contract-aware
binary; authority is never left declared where the contract's columns no
longer exist.

**Rollback / restart.** The table rebuild, the backfill, the index creation, and
the `schema_capabilities` marker write all execute in a single transaction
(§4.2 steps 1–7). The connection-level foreign-key prelude and epilogue sit
outside that transaction by necessity: the suppression must be in effect before
`BEGIN`, and it is restored — with a verified read-back — after the transaction
ends, on the success, error, and panic paths alike (§4.2 steps 0 and 8). A
crash or power loss leaves either the fully pre-migration or the fully
post-migration schema; the pragma is per-connection state, not stored data, so
it never persists into the file. A restart re-runs idempotently. No partially
activated intermediate schema is observable to a reader.

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
- **Root reauthorization** keeps the permanent root boundary (§1, §7): the
  destination **root** itself must remain a valid UTF-8 absolute path, because
  it is persisted through the lossy `library_roots.path` key that this contract
  explicitly does not govern (§10). The existing refusals
  (`engine.rs:1180-1182,1233`) are retained unchanged for the root; only
  descendant track/playlist paths inside the reauthorized root gain
  native-codec retargeting. A reauthorization whose destination root is not
  valid UTF-8 is refused with a closed diagnostic (§8.14) and no rows move; it
  is never retargeted through the native codec, which would encode the
  destination losslessly for descendants while the root row itself collapses
  distinct invalid-byte roots in `library_roots.path` via `to_string_lossy`.

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
  **fail closed**: encountering any non-authoritative row (U+FFFD display text,
  `native_path_state` 0 or 3, or `native_path IS NULL`) aborts the whole
  export with an `Err` **before** serialization or atomic persistence begins,
  leaving the destination file unchanged; the existing UI `Err` handling
  surfaces the failure. Export never omits non-authoritative rows into a
  silently incomplete playlist artifact, and it never emits a lossy
  `display_path` location. This is part of the §7 baseline and must land
  before schema/scanner activation, so an intermediate build can never export
  a false locator for a newly enrolled row.
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
  location. Any non-authoritative row makes the whole export fail closed with
  an `Err` before serialization or atomic persistence begins, leaving the
  destination file unchanged — rows are never omitted into a silently
  incomplete artifact (§6.3); authoritative rows are emitted only from a
  decoded native key. Import accepts a location
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
    `native_path_authority_version` startup guard refuses a contract-aware
    binary compiled below the database's authority version; `down()` refuses
    while any row would be ambiguous under the old schema and otherwise
    restores the `file_path` column name, deletes the `schema_capabilities`
    authority singleton row, and resets the mirrored `PRAGMA user_version` to
    the pre-R11 value — assert that after a successful `down()` the
    `schema_capabilities` authority row is absent and `PRAGMA user_version`
    equals the captured pre-R11 value (the numeric marker is restored, not
    absent), so the downgraded database presents as pre-R11 to every
    contract-aware binary (§4.5). Assert the containment scope
    symmetrically: a `SELECT *` and a read naming only surviving columns
    succeed against the rebuilt table, demonstrating that the mechanical
    barrier covers statements naming the removed column, not arbitrary
    pre-R11 statements (§4.5). Assert
    row ids, ratings, play counts, history, and playlist references are
    byte-identical across the upgrade.
12. **Intermediate-version integration matrix (F2).** For every independently
    landable slice state (R11a baseline only; R11a+R11b; R11b without R11c;
    R11c without R11d), run the resolver, tag-write, export, and stale-removal
    paths against (a) an invalid-byte row whose display contains U+FFFD,
    (b) a literal replacement-character row, and (c) an authoritative native
    row before its consumer has shipped; assert a closed refusal with no
    wrong-file open, no write, and no lossy export.
13. **Migration reference preservation and failure injection (rebuild gate).**
    Run migration `000021` against a populated database: several `tracks` rows
    covering states 2 and 3, live `playlist_entries` rows whose non-NULL
    `local_track_id` values reference them, plus ratings, play counts, and
    history. Assert after the migration: playlist binding rows are unchanged —
    exact `(playlist_entries.id, local_track_id)` pair-set equality, so no
    binding became NULL and no two entries' bindings were swapped — track
    ids/ratings/play counts/history are
    byte-identical (the migration-grain form of §8.11's cross-upgrade
    assertion), and `foreign_key_check` is empty. Additionally assert the
    failure paths: (a) a connection where `PRAGMA foreign_keys = OFF` does not
    take effect (read-back not `0`) aborts before `BEGIN` with no writes; (b)
    an injected mid-rebuild failure (unique-index violation during the copy)
    rolls the transaction back to a byte-identical pre-migration database with
    all playlist bindings intact; (c) the migration code path never issues
    `PRAGMA legacy_alter_table`; and (d) the connection epilogue restores
    foreign-key enforcement with a verified read-back, including when the
    migration body returns an error or panics.
14. **Root reauthorization boundary (§5.5).** A library-root reauthorization
    whose destination root is not valid UTF-8 is refused with a closed
    diagnostic before any row is moved; no `library_roots` row is written with
    a lossy destination and no track rows are retargeted. Reauthorization of
    descendants inside a valid UTF-8 destination root proceeds through the
    native codec as §5.5 specifies.

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
   wrong delete/reassign); XSPF export fails closed with an `Err` on any
   U+FFFD or non-authoritative location instead of emitting a false locator or
   a silently incomplete artifact, and import refuses unprovable locations; the UI
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
   switches from the baseline whole-export `Err` refusal to lossless emission
   only for authoritative rows; decide and document the Rhythmbox non-UTF-8
   scope.
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

Each acceptance item, with the sections where it is satisfied:

- Versioned reversible representation or explicit boundary — §1, §3, §7
- Display text separated from authoritative identity (enforced by the column
  rename) — §1, §3.4, §4.1, §4.2, §5, §6.4
- Preserve IDs/history/ratings/playlists only where exact identity is provable —
  §2.1, §4.2, §4.3, §5.4, §8.4a, §8.13
- Quarantine ambiguous legacy rows, no guessing (heuristics withdrawn) — §4.3,
  §4.4, §5.4, §8.4a
- Scanner lookup/reconciliation, migration, playback, tag writes, import/export —
  §4, §5, §6
- Linux fixtures: invalid bytes, literal replacement collisions,
  Unicode/normalization, rename — §8
- Safe rejection/diagnostics until lossless support — §7, §8.14
- No lossy consumer exposed to newly encoded rows (fail-closed-first) — §7, §9,
  §8.12
- Older-binary / version-guard containment (no false authority re-enabled) —
  §4.2, §4.5, §8.11

## 13. Corrective revision mapping (independent review of `16140be0`)

This revision answers the three P1 findings in
`refinery-20260917-tr-ldhwt/corrective-instructions.md`. For each finding: the
required correction, where the design changed, and the validation added.

### F1 — single-candidate size/mtime legacy adoption guesses identity

- **Required correction:** Adoption must rest on independently retained exact
  evidence, or the row stays quarantined and the observed file enrolls fresh;
  align the state machine, acceptance mapping, and fixture 4; add the
  missing-original/single-stranger trace
- **Where changed:** Withdrew the §5.4 one-to-one size/mtime rule; made state 3
  terminal absent an exact retained key; updated the §4.4 machine, §12 mapping,
  and §8.4 fixtures
- **Validation added:** §8.4a mandatory non-adoption trace; §8.4b
  exact-key-only promotion

### F2 — staged rollout exposes lossy consumers to newly lossless rows

- **Required correction:** Every authority consumer must fail closed first, or
  activation and consumer switches must be atomic; specify export handling and
  rollback/restart; add intermediate-version integration tests
- **Where changed:** Split out the §7 fail-closed baseline as R11a and gated
  schema/scanner activation (R11b) on it; made export refusal part of the
  baseline; specified the single-transaction marker write and restart behavior;
  added the §9 intermediate matrix
- **Validation added:** §8.12 per-slice refusal matrix; §8.6 stale-removal
  refusal; §9 ordering

### F3 — older-binary compatibility re-enables false authority

- **Required correction:** A real supported-version/startup guard or a
  demonstrable compatibility barrier; remove the SQL-compatibility claim; add
  an old-binary/open-database rejection trace; separate downgrade from
  reopening
- **Where changed:** Rewrote §4.5: `schema_capabilities`/`user_version` guard,
  `file_path` → `display_path` carrier removal, no compatibility claim,
  `down()` only as downgrade, transactional rollback
- **Validation added:** §8.11 old-binary rejection trace and intact-history
  assertion

Nothing in this revision expands product behavior; every change makes the
contract stricter. The mechanical check results recorded at `16140be0` remain
historical; the next independent re-review should evaluate this corrected head.

### 13.1 Revision 3 mapping (independent review of `bc49dbe5`)

Revision 3 answers the migration reference-loss finding in
`refinery-20260917-tr-ldhwt-migration/corrective-instructions.md`
(exact head `bc49dbe514334e94c081e3156ab97a12df11f066`). Fields: the finding,
the required correction, where the design changed, and the validation added.

#### M1

- **Finding:** the §4.2 rebuild toggled `foreign_keys` inside the transaction
  (a change SQLite silently ignores) or proposed `legacy_alter_table` semantics
  (which do not disable `ON DELETE` actions), so `DROP TABLE tracks` fired
  `playlist_entries.local_track_id ON DELETE SET NULL` undetected;
  `foreign_key_check` stayed empty because NULL is legal
- **Required correction:** Specify an executable connection/transaction
  sequence: FK suppression set and read-back verified on the dedicated
  connection **before** `BEGIN`; populated preservation plus
  `foreign_key_check` gates inside the transaction before `COMMIT`; restore
  with verified read-back on every exit path; `legacy_alter_table` banned with
  the reproduction rationale; require populated preservation and
  rollback/failure tests, not only `foreign_key_check`
- **Where changed:** Rewrote §4.2 as steps 0–8 (connection prelude,
  in-transaction preservation gate, connection epilogue) with the explicit
  `legacy_alter_table` ban; extended the §4.5 rollback/restart contract;
  updated the §12 mapping
- **Validation added:** §8.13 populated preservation + failure-injection
   tests: pre-`BEGIN` abort on failed suppression, mid-rebuild rollback to a
   byte-identical database with bindings intact, no `legacy_alter_table`
   issued, verified FK restore on success/error/panic; §12 preservation row
   cites §8.13

### 13.2 Revision 4 mapping (round-2 review of `6218ec03`)

Revision 4 answers the five valid unresolved review threads verified in
`refinery-20260918-6218ec03-tr-ldhwt/corrective-round2-findings.md` (PR #283
at head `6218ec030b8c36055d1212c299e699304455318f`). All corrections are
doc-only; labeled R2-F1…R2-F5 to distinguish them from the §13 Revision-2
findings.

#### R2-F1 — root reauthorization dropped the permanent root UTF-8 boundary

- **Finding:** §5.5 instructed that root reauthorization "no longer needs to
  reject non-UTF-8 destinations" and the appendix routed it to the native
  codec, so an invalid-byte destination root would be accepted and persisted
  through the lossy `library_roots.path` key — re-creating the exact
  lossy-root collapse R11 removes. It contradicted the permanent §1 root
  boundary, §7, and the appendix's own "unchanged (root boundary)" rows.
- **Required correction:** the destination root keeps the explicit UTF-8
  refusal (it is a root, not a descendant); only descendant track/playlist
  paths gain native-codec retargeting; add the required refusal test note.
- **Where changed:** rewrote the §5.5 root-reauthorization bullet; corrected
  the Appendix A row to "unchanged: destination root keeps the UTF-8
  refusal"; added the §8.14 reauthorization-boundary test; §12
  safe-refusal row cites §8.14.
- **Validation added:** §8.14 — refusal of a non-UTF-8 destination root with
  no rows moved, and codec retargeting of descendants inside a valid UTF-8
  root.

#### R2-F2 — preservation gate could not detect swapped bindings

- **Finding:** the §4.2 step-5 gate compared only the `playlist_entries` row
  count and the multiset of non-NULL `local_track_id` values; swapping two
  entries' track bindings preserves both and would pass, contradicting the
  step's own "no binding may change".
- **Required correction:** snapshot and compare exact
  `(playlist_entries.id, local_track_id)` pairs before and after the rebuild;
  require pair-set equality before `COMMIT`, keeping the row-count, track
  id-set, and `foreign_key_check` clauses.
- **Where changed:** rewrote §4.2 step 5 around the exact binding-pair
  snapshot with explicit swap detection; strengthened the §8.13 assertion to
  the same pair-set equality.
- **Validation added:** §8.13 binding assertion now fails on any rebind,
  including swaps, not only on NULL-ing.

#### R2-F3 — state diagram implied migration writes state 1

- **Finding:** the §4.4 diagram's top arrow showed "migration →
  authoritative (1)" for a "(none)" origin, while §4.2 step 3 and §4.3
  guarantee the migration never writes state 1 — backfill terminates in
  state 2 (U+FFFD-free) or state 3 (U+FFFD).
- **Required correction:** relabel the diagram: the backfill path terminates
  in state 2 (state 3 for U+FFFD); state 1 is entered only by exact-key
  re-observation of a state-2 row, an exact retained key for a state-3 row,
  or new post-migration enrollment; no direct migration→1 arrow.
- **Where changed:** redrew the §4.4 diagram and added the explicit
  "migration never writes state 1" paragraph beneath it.
- **Validation added:** none beyond the diagram/text consistency; §4.2
  step 3, §4.3, and §4.4 now state the same transition set.

#### R2-F4 — older-binary containment overstated

- **Finding:** §4.5 claimed "two independent mechanisms enforce" that a
  pre-R11 binary is rejected, but the version guard is read by contract-aware
  binaries only, and carrier removal fails statements only when they name
  `file_path` (`SELECT *` and reads of surviving columns prepare and execute
  fine). No executable barrier rejects an arbitrary pre-R11 binary.
- **Required correction:** narrow the claim to what the mechanisms deliver —
  version-check rejection of contract-aware older binaries, and
  statement-preparation failure for anything naming `file_path` — and state
  plainly that an arbitrary pre-R11 binary is unsupported-but-not-
  mechanically-rejected unless it names the removed column.
- **Where changed:** rewrote the §4.5 intro and both mechanism descriptions
  with the exact scope; aligned §1's carrier-removal sentence; updated the
  §8.11 trace to assert the scope symmetrically (`SELECT *` succeeds).
- **Validation added:** §8.11 now asserts both the barrier (statements naming
  `file_path` fail) and its boundary (statements not naming it succeed).

#### R2-F5 — export "omits" contradicted the fail-closed baseline

- **Finding:** §6.3 allowed export to "omit or diagnose" non-authoritative
  rows; omission silently produces an incomplete playlist artifact, which is
  not failing closed and contradicts §1, §4.4, and the §7 baseline.
- **Required correction:** encountering any non-authoritative row makes the
  whole export return an `Err` before serialization or atomic persistence
  begins, leaving the destination file unchanged; existing UI `Err` handling
  surfaces the failure; drop "omits".
- **Where changed:** rewrote the §6.3 export bullet, the §7 export-refusal
  bullet, and the §9 R11a slice description to the all-or-error contract.
- **Validation added:** §8.12's per-slice matrix already asserts "no lossy
  export"; its refusal is now specified as whole-export `Err` with the
  destination unchanged.

Nothing in this revision expands product behavior; every change makes the
contract stricter. Threads stay unresolved until this head lands and the five
fixes are independently re-verified.

## Appendix A — Lossy conversion inventory (`src/local/`)

Grounded at the design commit; implementation must convert each to the native
key. The `file_path` column is renamed `display_path` by this contract; the
entries below name the legacy identifier as it exists at the design commit.

| Location | Current lossy use | New authority |
| --- | --- | --- |
| `tag_parser.rs:169` | `ParsedTrack.file_path` | carry native key + display |
| `engine.rs:3892-3893` | scan dedup + `on_disk_paths` | native-key set/map |
| `engine.rs:2114,2184,2237` | dir-rename observed/destination keys | native-key set |
| `engine.rs:2318-2323` | stale membership | native-key test |
| `engine.rs:4759` | watcher removal lookup | native-key lookup |
| `engine.rs:5902,5936,6020` | dir-rename surplus / removal events | native key |
| `engine.rs:6400-6444` | file rename retarget | native key |
| `engine.rs:6504-6573` | dir rename prefix/join | native key |
| `engine.rs:1180-1182,1233` | root reauthorization non-UTF-8 refusal | unchanged; see §5.5 |
| `engine.rs:2753,2868,2953,2989` | `library_roots.path` keys | unchanged (root boundary) |
| `engine.rs:3647,3705,4343,4356` | `library_roots.path` keys | unchanged (root boundary) |
| `resolver.rs:353` | playback `PathBuf::from(file_path)` | decode `native_path` |
| `playlist_io.rs:89,732,754-766` | XSPF location encode/decode | lossless mapping |
| `playlist_io.rs:591-700` | import match index | native key, then fingerprint |
| `rhythmbox_import.rs:1534` | non-UTF-8 refusal | R11d decision |
| `rhythmbox_migration.rs:1026-1264` | path string matching | native key |
| `ui/window.rs:4622-4624` | display-URI tag target | id-keyed lookup |
| `ui/context_menu.rs:1349-1354` | display-URI tag target | id-keyed lookup |
