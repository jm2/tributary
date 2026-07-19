# Track rating contract

Last audited: 2026-07-19

This document defines Tributary's rating value, ownership, source capabilities, persistence, and
playlist-interchange boundaries. It covers the completed P1.4 foundation. The visible column,
accessible editing, sorting, and smart-playlist rules remain the second P1.4 implementation record.

## Canonical value

- A rating is one whole integer from **1 through 100**, inclusive.
- `None` means unrated. Zero is not an alternate spelling for unrated.
- Invalid values cannot enter the architecture model. The local SQLite constraint independently
  rejects out-of-range and non-integer storage.
- The scale is deliberately presentation-neutral. A later UI may display stars or another
  accessible control, but its value must round-trip through the exact 1–100 integer.

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
is rejected. Down/up retry removes any rating with the column and recreates legacy rows as unrated.

`LocalBackend::set_track_rating` sets or clears one exact source-native `TrackId` in a transaction,
selects the replacement row before commit, and returns only committed state. A missing row is a
clean no-op; the primary key prevents a write from affecting more than one row. The type and schema
both validate the value.

Ratings are Tributary application data. Metadata refreshes of an existing database row and
recognized paired watcher renames (including directory renames) preserve them. A full scan matches
tracks by exact `file_path`, so an offline rename or other unrecognized remove-plus-add is a new
unrated track and the stale rated row is removed. Tributary neither reads a local rating from an
embedded audio tag nor writes one back to a media file. This avoids format-dependent tag loss and
unrequested file modification.

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

Malformed non-finite or out-of-range Jellyfin/Plex values become read-only unrated state. They are
not clamped into plausible user data. DAAP's current requested metadata, Radio-Browser, removable
files, and external files expose no unambiguous native rating, so they remain `Unsupported`.

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
transactional local set/clear/exact-ID behavior; rating preservation across metadata refresh and
renames; XSPF omission; inert rating-like extension input; and rating-neutral playlist matching.
