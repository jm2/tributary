# Track rating contract

Last audited: 2026-07-19

This document defines Tributary's complete P1.4 rating contract: value and ownership, source
capabilities, persistence, visible presentation and editing, ordering, smart-playlist behavior, and
playlist-interchange boundaries.

## Canonical value

- A rating is one whole integer from **1 through 100**, inclusive.
- `None` means unrated. Zero is not an alternate spelling for unrated.
- Invalid values cannot enter the architecture model. The local SQLite constraint independently
  rejects out-of-range and non-integer storage.
- The tracklist presents that exact integer in a **Rating** column. It does not round to stars or
  collapse distinct stored values onto a coarser display scale.

## Ownership and capabilities

Every published track carries one coherent `TrackRating` state:

| Capability | Meaning | Current sources |
|---|---|---|
| `Writable` | Tributary owns the optional value and may persist a change. | Built-in local library only |
| `ReadOnly` | The source exposes a trustworthy optional value, but Tributary does not write it. | Subsonic, Jellyfin, Plex |
| `Unsupported` | No trustworthy value is published and no mutation is allowed. | DAAP, Radio-Browser, removable media, operating-system-opened files, unknown future adapters by default |

The `MediaBackend` seam also publishes a source-wide capability. Catalogue admission rejects the
entire result if any track's capability disagrees with its backend, preventing UI policy from
silently drifting from adapter policy. The default backend capability and mutation implementation
are `Unsupported`, so a new or incomplete remote adapter fails closed.

## Local persistence

Migration 12 adds `tracks.rating` as a nullable SQLite `INTEGER` with a constraint requiring either
`NULL` or integer storage in the inclusive 1–100 range. It has no default: every legacy row becomes
`NULL`/unrated without inference from tags, play counts, dates, or playlists. Interrupted upgrades
accept only the exact compatible nullable and constrained shape; a lookalike unconstrained column
is rejected, while the exact definition remains recognizable after later columns are appended.
Down/up retry removes any rating with the column and recreates legacy rows as unrated.

`LocalBackend::set_track_rating` sets or clears one exact source-native `TrackId` in a transaction,
selects the replacement row before commit, and returns only committed state. A missing row is a
clean no-op; the primary key prevents a write from affecting more than one row. The type and schema
both validate the value.

All UI rating writes enter the local-library engine as `SetTrackRating` commands on the same
unbounded, serialized FIFO used for playback-history and root-trust mutations. The command carries
the exact local `TrackId` and either a validated `Rating` or `None`; it never identifies a row by
URI, display metadata, or the compatibility UUID. Normal shutdown closes command admission before
appending its flush marker, so every rating command admitted before shutdown completes ahead of the
flush and nothing can be queued behind it.

The tracklist is not changed when a command is merely admitted. After `LocalBackend` commits, the
engine publishes the complete replacement `Track`; GTK replaces the matching local row by exact
track ID, lets an active Rating sort reorder it, and invalidates playlist projections so a visible
smart playlist is reevaluated against committed state. There is no optimistic rating to roll back.
A row deleted before its command reaches storage remains a clean no-op and publishes no replacement.
If persistence fails, the detailed database error is confined to the internal log and track
identity is recorded only through `TrackId`'s redacted debug representation. The UI-facing event
carries the typed exact track ID, and GTK reports the fixed localized “could not update” status;
raw SQL, paths, native IDs, and driver text cannot enter user-visible copy.

Ratings are Tributary application data. Metadata refreshes of an existing database row and
recognized paired watcher renames (including directory renames) preserve them. A full scan matches
tracks by exact `file_path`, so an offline rename or other unrecognized remove-plus-add is a new
unrated track and the stale rated row is removed. Tributary neither reads a local rating from an
embedded audio tag nor writes one back to a media file. This avoids format-dependent tag loss and
unrequested file modification.

## Rating column, editing, and preferences

When present in a source view, the resizable, sortable **Rating** column represents every
`TrackRating` state explicitly:

| Track state | Cell presentation | Editing |
|---|---|---|
| `Writable(Some(value))` | Exact integer from 1 through 100 | Enabled |
| `Writable(None)` | Localized **Unrated** text | Enabled |
| `ReadOnly(Some(value))` | Exact integer plus a localized read-only indication | Disabled |
| `ReadOnly(None)` | Localized **Unrated (read-only)** indication | Disabled |
| `Unsupported` | Localized **Unavailable** indication | Disabled |

Writable cells expose a popover with a localized, accessible “rating from 1 to 100” label, an
integer spin control constrained to 1 through 100 in one-point steps, and localized Apply and Clear
actions. A rated row opens at its committed value; an unrated row starts the input at 100. Apply
queues the validated integer, while Clear queues `None`. Nonnumeric cell-state text,
tooltip/accessibility copy, action labels, and failure status have translations in every shipped
locale. Read-only and unsupported rows never enable the popover or queue a mutation.

Radio-Browser's deliberately compact station view omits Rating together with other track-only
metadata instead of filling a whole column with the same Unavailable state. Its rows remain
`Unsupported`, so any future presentation outside that specialized view still fails closed.

The column exists independently of whether it is currently visible. Fresh profiles include it in
the default all-columns-visible layout. Configuration schema version 1 performs a one-time upgrade
for established profiles that predate Rating: it inserts the column after Plays when possible
(otherwise at the end), exposes it once, preserves the relative order and visibility of every
existing column, and records the version. Reapplying the migration is idempotent. Once a profile is
current, an intentionally hidden or reordered Rating column remains hidden or reordered across
loads instead of being forced visible again.

## Ordering

Clicking the tracklist Rating header sorts rated rows by their canonical integer. Rated rows always
precede missing values in both ascending and descending order. Within the missing group,
readable-but-unrated rows precede `Unsupported` rows, preserving the distinction between “no value”
and “no rating capability.” Equal values use the exact track ID as a deterministic final tie-break;
the tie direction follows the active ascending or descending UI order.

Smart-playlist compound ordering also offers Rating in either direction. It orders present values
numerically and keeps both readable-unrated and unsupported values last in both directions. If the
configured compound order contains Rating, the exact track ID is the final ascending tie-break
after all requested criteria, making reevaluation deterministic even when values and other sort
keys are equal.

## Smart playlists

The editor exposes **Rating (1–100)** with these operators:

- `is`, `is not`, `greater than`, and `less than` take one canonical integer;
- `in range` takes an inclusive low and high integer; and
- `is rated` and `is unrated` are explicit presence predicates and need no user-entered value.

Numeric operators apply only to a readable, present rating. In particular, `is not` does not turn
an absent rating into a match. Both `Writable` and `ReadOnly` values participate identically in
numeric and presence evaluation. `IsRated` matches either readable capability with a value;
`IsUnrated` matches either readable capability without a value. `Unsupported` matches neither
presence predicate and no numeric predicate, so an incapable source is never mislabeled as
unrated.

The editor validates without changing the user's text. A scalar must be in 1 through 100; both
range endpoints must be canonical and the low endpoint must not exceed the high endpoint. Empty,
non-numeric, out-of-range, and reversed input receives localized visible and accessible feedback,
and OK remains disabled until every rating row is valid. Nothing is clamped or guessed. Evaluation
repeats the same bounds checks defensively, so a malformed or wrong-shaped serialized rule from
outside the editor matches no track. Presence rules store a canonical inert placeholder for
backward-compatible serialization; evaluation requires that exact `Number(1)` shape and then
otherwise ignores it.

Result limits can select **Highest Rated** or **Lowest Rated**. Present ratings determine membership
first, missing ratings remain last, and equal ratings use ascending exact track ID as the stable
tie-break. The separate Rating compound-sort criterion then orders the selected subset according to
the configured direction. Saving, reopening, and resaving a smart playlist preserves rating fields,
all seven operators, inclusive ranges, presence predicates, limit selection, and compound sort.
Committed local rating changes invalidate and reevaluate active playlist projections immediately.

## Read-only remote conversion

Tributary does not send rating mutations to any remote server.

- **Subsonic:** an optional signed `userRating` is accepted only when it is an integer from 1
  through 5, then maps exactly to 20, 40, 60, 80, or 100. Negative, zero, or greater-than-five
  values are treated as read-only unrated data rather than rejecting the catalogue or guessing.
- **Jellyfin:** optional `UserData.Rating` uses the server's decimal 0–10 scale. Only finite values
  inside that range are accepted. Tributary computes `round(value × 10)`; a valid native zero maps
  to canonical 1 so it remains rated, while a missing value remains unrated.
- **Plex:** optional `userRating` follows the same validated decimal 0–10 conversion as Jellyfin.
  Plex `ratingKey` remains only the item's identifier and is never interpreted as a rating.

Malformed, numerically unrepresentable, non-finite, or out-of-range Jellyfin/Plex values become
read-only unrated state without rejecting the surrounding response. They are not clamped into
plausible user data. DAAP's current requested metadata, Radio-Browser, removable files, and external
files expose no unambiguous native rating, so they remain `Unsupported`.

## XSPF and imports

XSPF version 1 has no standard track-rating field and Tributary's playlist workflow has no consent
or conflict-resolution step for modifying library metadata. Therefore:

- XSPF export intentionally omits app-owned and read-only ratings.
- Import ignores rating-like generic `<meta>` elements and extension content.
- Matching an imported playlist row never changes the matched local track's rating.

This asymmetry is intentional: an XSPF file describes playlist membership, not catalogue
authority. A future rating-transfer feature would need an explicitly versioned scalar plus a
separate opt-in metadata-import flow with source ownership, conflicts, previews, and transactional
rollback; it must not be smuggled into ordinary playlist import.

## Validation matrix

Automated regressions cover canonical and serialized value boundaries; signed Subsonic and decimal
Jellyfin/Plex conversions, including malformed values; source/track capability agreement; default
remote mutation refusal; migration shape, legacy defaults, constraints, retry, and rollback;
transactional local set/clear/exact-ID behavior; serialized command admission, committed-only
publication, missing-row no-op, fixed failure events, and shutdown FIFO ordering; live local
replacement and playlist invalidation; rating preservation across metadata refresh and renames;
localized writable/read-only/unavailable presentation in every shipped locale; tracklist null-last
primary/secondary ordering and exact-ID ties; editor-side no-clamp validation plus localized
visible/accessibility errors, defensive numeric/range/presence evaluation, limit selection, and
compound smart-rule semantics against persisted rows; one-time column-config evolution and
current-profile hidden-column preservation; XSPF omission; inert rating-like extension input; and
rating-neutral playlist matching.
