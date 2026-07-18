# Tributary implementation roadmap

Last audited: 2026-07-18

This document explains the product and engineering work that remains **after** the holistic-review
remediation. [`task.md`](task.md) is the countable active implementation backlog; the completed
remediation record is preserved separately in
[`task-remediation-2026-07.md`](task-remediation-2026-07.md) at **220/223 (98.7%)**, with only three
real-environment validation records left. The new feature backlog starts at **0/35 (0%)**. Neither
percentage estimates equal engineering effort, and the historical percentage is not a claim that
Tributary has implemented every requested product feature.

The entries below are candidates, not release promises. As of this audit, all 11 open GitHub
issues are unlabeled, unassigned, have no milestone, and have no discussion establishing product
priority. An issue should receive acceptance criteria, dependencies, and a milestone before work
starts. Historical holistic-review documents are point-in-time findings, not active roadmaps.

## Current baseline

- The P0-P3 implementation remediation is complete. The remaining in-scope tracker records require
  physical removable hardware, an installed interactive Flatpak environment, or packaged Windows
  playback against live DAAP and Subsonic servers.
- Local, Subsonic, Jellyfin, Plex, and DAAP publish complete catalogues through the shared
  `MediaBackend` seam. Connected remotes, Radio-Browser, removable media, and operating-system-opened
  files use the common `SourceRegistry` lifecycle and playback-time authority model.
- AirPlay 1/RAOP, Chromecast, MPD, and local playback are implemented. AirPlay 2/HomeKit is not.
- Regular playlists are local-library projections. Remote tracks cannot currently be persisted in
  them, and Subsonic server-side playlists are not imported.
- XSPF v1 import/export is implemented with exact path and deterministic normalized-metadata
  matching. Apple/iTunes XML, Google Takeout CSV, M3U, service URLs, and fuzzy matching are not
  direct input modes.
- Mounted removable filesystems can be browsed and played. Copy/sync, MTP-only devices, automount,
  eject, and pathless removable tag mutation are not implemented.

## Proposed implementation order

This order favors correcting misleading current interactions and building shared foundations
before starting large protocol or transfer subsystems.

### 1. Correct the playlist and playback-history contracts

1. **Make shuffled navigation follow real history.** `PlaybackSession` already keeps visited queue
   indices, but the user-facing Previous/Next path needs an end-to-end regression and bounded
   repeat-all semantics. Previous should walk actual queue occurrences, Next after Previous should
   walk forward history before drawing another random occurrence, and the fixed history budget
   should retain the current occurrence plus ten real predecessors without weakening per-cycle
   no-repeat behavior.
2. **Make remote-to-playlist behavior explicit ([#47]).** The current Add to Playlist action is
   offered for remote selections, but unsupported rows are only counted in a log message. The
   smallest slice is a user-visible refusal. Full support is separate: regular playlist entries
   need persistent source-scoped `(SourceId, TrackId)` identity, disconnected-source semantics,
   and playback-time resolution; Subsonic server-native playlist import/sync is another slice.
3. **Implement trustworthy local playback history.** The database and UI expose `play_count`, but
   production playback does not increment it and there is no `last_played` field. Consequently the
   seeded Recently Played and Top 25 playlists are present but ordinarily empty for local music.
   Define the counted-play threshold, repeat/seek behavior, durable update path, and smart-playlist
   refresh semantics before building features that consume this history.
4. **Add ratings ([#37]).** Decide whether ratings are app-local, written to tags, synchronized to
   capable servers, or some explicit combination. Then add schema/model/backend propagation, an
   editable and sortable column, smart-playlist rules, and mixed-capability behavior.

These foundations make Rhythmbox migration and Last.fm behavior much less ambiguous.

### 2. Build migration and listening integrations

1. **Rhythmbox migration ([#57]).** Import `rhythmdb.xml`, playlists, play counts, and ratings
   transactionally and idempotently; match files without guessing; and report conflicts and
   unmatched rows. Existing XSPF import is useful but is not automatic Rhythmbox migration.
2. **Last.fm scrobbling ([#50]).** Design account authorization and secret storage, now-playing and
   scrobble thresholds, a durable retry queue, offline behavior, privacy disclosure, and
   source-aware metadata. This should consume the same authoritative playback-history events.

### 3. Add bounded library-management UX

1. **Local playlist and file drag-and-drop ([#46]).** Start with multi-selection drops onto local
   playlists. File-manager export, remote tracks, and device copies have different authority and
   transfer semantics and should be separate follow-ups.
2. **Browse by folder ([#14]).** Define root-aware relative directory identity, multiple-root
   disambiguation, lazy navigation, and rename/unavailable-root behavior. Pathless remote and
   removable sources need an explicit supported-or-omitted policy.
3. **Album art in the browser ([#39]).** Choose an art-enhanced list or virtualized album grid, then
   add bounded asynchronous loading, caching, cancellation, placeholders, authenticated artwork,
   accessibility, and persisted layout preferences.
4. **UI refinements ([#29]).** The requested separator, count-opacity, and alignment changes remain
   open. Confirm each subjective change against the current GNOME HIG and themes before applying
   it, and review the result visually.

### 4. Plan audio-output work explicitly

1. **Equalizer ([#49]).** Define the GStreamer filter graph, bands/presets, preamp and clipping
   policy, live reconfiguration, persistence, and per-output behavior. Local and AirPlay pipelines
   can potentially process locally; Chromecast and MPD may need receiver-side support or an
   explicit unsupported state.
2. **AirPlay 2/HomeKit sender.** Complete a design investigation before choosing a dependency or
   implementation. At minimum this requires pairing, encrypted control, and the expected encoded
   audio/timing path. Multi-device clock synchronization is required only if simultaneous
   multi-room output becomes an explicit goal. See [AirPlay 2](#airplay-2).
3. **Chromecast IPv6 publication.** The current receiver-facing ticket listener is IPv4-only, so an
   IPv6-only Cast control endpoint is omitted rather than given an unreachable media URL.
4. **MPD detectable exclusive-control mode.** Safe automatic orphan cleanup remains coupled to a
   stronger ownership mode that must also account for MPD's partition-global pause, stop, repeat,
   random, single, and consume operations. The current explicit exclusive-control confirmation and
   conservative orphan retention remain the safe behavior until then.

### 5. Treat data movement as design epics

1. **Offline remote cache/download ([#11]).** This needs authenticated resumable downloads,
   source-scoped persistent cache identity, atomic files, quota/eviction, offline catalogues,
   reconciliation, progress/cancellation, and a clear credential and licensing policy.
2. **Android/device synchronization ([#8]).** Mounted-filesystem browsing is only a foundation.
   A real sync feature needs write authority, capacity/conflict planning, incremental state,
   playlist mapping, progress/cancel/rollback, optional auto-sync, and MTP support for typical
   Android devices.
3. **Typed removable mutation authority.** Re-enabling Properties for pathless removable rows
   requires retained, revalidated write authority and safe replacement semantics. Until that
   exists, omitting Properties is intentional and safer than reconstructing a host path.

## Live open issues

This is a snapshot of the open issue set on 2026-07-18. GitHub remains authoritative for whether an
issue is open; this table records the implementation assessment so a feature request is not
mistaken for work already underway.

| Issue | Current implementation state | Likely implementation shape |
|---|---|---|
| [#57 — Rhythmbox playlists, play counts, and ratings](https://github.com/jm2/tributary/issues/57) | No direct importer. XSPF conversion and a dormant local play-count field are only partial foundations. | Playback history and ratings first; then transactional, idempotent migration with conflict reporting. |
| [#50 — Last.fm scrobbling](https://github.com/jm2/tributary/issues/50) | No Last.fm client or scrobble pipeline. | Authorization, secret storage, authoritative thresholds, retry/offline queue, and privacy UX. |
| [#49 — Equalizer](https://github.com/jm2/tributary/issues/49) | No equalizer or audio-filter configuration. | GStreamer DSP design plus explicit behavior for every output backend. |
| [#47 — Remote/Subsonic tracks in playlists](https://github.com/jm2/tributary/issues/47) | Remote rows are skipped by the local-only playlist writer; the failure is not user-visible. | Immediate UX fix, then source-scoped playlist schema/resolution; server playlist sync separately. |
| [#46 — Drag and drop](https://github.com/jm2/tributary/issues/46) | Column-header reordering exists; track/file drag-and-drop does not. | Local playlist DnD first; file export, remote rows, and device copies as distinct policies. |
| [#39 — Album art in browser](https://github.com/jm2/tributary/issues/39) | Artwork is shown for now-playing, not in the Genre/Artist/Album browser. | Virtualized art UI with bounded async cache, cancellation, accessibility, and authenticated art. |
| [#37 — Rating column](https://github.com/jm2/tributary/issues/37) | No rating field in the core track model, database, or track list. | Define ownership/sync semantics, then schema, editing, sorting, rules, and backend capabilities. |
| [#29 — UI refinement](https://github.com/jm2/tributary/issues/29) | Requested separators/alignment changes are not implemented. | Split into independently reviewable visual changes after current-theme design review. |
| [#14 — Browse by folder](https://github.com/jm2/tributary/issues/14) | Browser panes expose Genre, Artist, and Album only. | Root-relative folder model and lazy UI with multiple-root and unavailable-root semantics. |
| [#11 — Offline cache/download](https://github.com/jm2/tributary/issues/11) | No remote download or offline catalogue subsystem. | Large persistent cache/download epic with quota, retry, reconciliation, and secure auth handling. |
| [#8 — Android synchronization](https://github.com/jm2/tributary/issues/8) | Mounted-device browse/play exists; transfer, sync, automount, and MTP do not. | Large transfer/sync epic with MTP, write authority, planning, progress, conflicts, and rollback. |

## AirPlay 2

AirPlay 2 receivers advertise via `_airplay._tcp.local.` and discovery labels them as `airplay2`,
but the UI deliberately filters them out because Tributary currently has only an AirPlay 1/RAOP
sender through GStreamer's `raopsink`.

No sender dependency or protocol implementation has been selected. The implementation must first
confirm current reverse-engineering and interoperability details for:

1. receiver pairing and authenticated session establishment;
2. the encrypted control channel;
3. codec, transport, and timing requirements for audio; and
4. clock synchronization only if multi-device playback is included.

Candidate approaches are delegation to a maintained external sender, a pure-Rust in-tree or
`gst-plugins-rs` sender, or waiting for an upstream component to mature. The single-binary
distribution model, platform packaging, license, maintenance health, and real-device test matrix
must be part of that decision. This work should start with a design issue; it is not currently an
active implementation.

## Other explicit follow-ups and accepted limits

### Engineering follow-ups

- Re-evaluate the unmaintained `paste` and `proc-macro-error2` dependency paths and the inactive,
  lockfile-only RSA advisory by 2026-10-10 or the next release, and immediately if MySQL support is
  ever enabled.
- Remove the macOS GStreamer channel-cap workaround only after an upstream fix is available in the
  supported runtime floor and has been validated on affected multi-channel hardware.
- A direct end-to-end watcher-backlog/root-confirmation ordering harness would strengthen existing
  component and engine-loop coverage, although the remediation acceptance record is already closed.

### Deliberate current limitations, not scheduled commitments

- Tributary does not automount or eject volumes and cannot browse pathless/MTP-only devices.
- Markerless read-only library roots cannot be enrolled because Tributary cannot create the durable
  identity marker required by the current fail-closed trust model.
- Direct Apple/iTunes XML, Google Takeout CSV, M3U, and service-specific playlist imports are not
  implemented. The documented conversion to XSPF is the current interoperability path, and fuzzy
  “similar name” matching is intentionally avoided.
- OS-open delivery admits the first valid playable candidate. A multi-file ephemeral queue is a
  possible future extension, not a committed feature.
- A stronger platform-native removable file identifier and explicit saved-server endpoint rebind
  are possible future schema extensions, not scheduled work.
- Proper Apple code signing/notarization and the intentionally deferred release-workflow exercise
  are distribution/release work, not product implementation in this roadmap.

## Keeping this roadmap current

When an item becomes active:

1. create or refine its GitHub issue with scope, acceptance criteria, non-goals, and dependencies;
2. assign a priority and milestone rather than treating this proposed order as a commitment;
3. link the design document when protocol, schema, authority, or cross-output behavior is involved;
4. update this roadmap, README feature status, and `CHANGELOG.md` in the implementing PR; and
5. close or narrow the issue only when the shipped behavior and documentation match.

[#8]: https://github.com/jm2/tributary/issues/8
[#11]: https://github.com/jm2/tributary/issues/11
[#14]: https://github.com/jm2/tributary/issues/14
[#29]: https://github.com/jm2/tributary/issues/29
[#37]: https://github.com/jm2/tributary/issues/37
[#39]: https://github.com/jm2/tributary/issues/39
[#46]: https://github.com/jm2/tributary/issues/46
[#47]: https://github.com/jm2/tributary/issues/47
[#49]: https://github.com/jm2/tributary/issues/49
[#50]: https://github.com/jm2/tributary/issues/50
[#57]: https://github.com/jm2/tributary/issues/57
