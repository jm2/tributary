# Tributary roadmap

Last reviewed: 2026-09-23.

This page describes where Tributary is heading. It is a direction, not a release promise.
**GitHub issues are the source of truth** for scope, acceptance, and state;
[`task.md`](task.md) maps older stable IDs (R1, Q4, P2.3-B, …) to issues. The README lists
what works today, and the [changelog](../CHANGELOG.md) lists what changed.

## How work is chosen

- **Prefer the smallest complete change.** The 2026-09-23 review found the project heavier than
  its features need: large subsystems that were never wired into the app, and process documents
  that drifted from the code. A feature should land end to end (behavior, tests, README, and a
  short changelog entry) in a reviewable PR, or wait.
- **Talk about risky changes on the issue first.** Changes to a network protocol, the database
  schema, credential handling, or privacy need a short design note on the issue (or a short doc
  under `docs/`) before code.
- **Link the issue.** Implementing PRs say `Closes #N` or `Refs #N`; closing the issue is how the
  work is marked done.

## Now: finish the 2026-09-23 review fixes

The two burn-down merge trains (#371 and #382) fixed most of the review's user-facing defects.
The remaining findings are open issues:

- **Library and folders:** folder entries going stale (#253), untranslated browser headings
  (#257), non-UTF-8 file names (#258), library folders that appear or are remounted after startup
  (#323), local engine edge cases (#352), and database upgrade robustness (#354).
- **Interface:** hard-coded English (#345), the smart-playlist editor (#346), Properties and
  MusicBrainz (#347), server dialogs (#348), settings persistence (#349), the context menu (#350),
  main-thread stalls (#351), and the remaining now-playing items (#344).
- **Outputs and servers:** Chromecast and AirPlay leftovers (#343), remote backend defects
  (#355), and media-relay and catalogue resource limits (#299, #300).
- **App lifecycle:** unsupported links, files opened while the window closes, macOS startup
  failures, and shutdown signals (#356); the Windows single-instance guard (#342).
- **Last.fm before it is switched on:** #335, #336, and #337.
- **Build, packaging, and CI:** required checks (#339), bundled codecs and license notices (#340),
  release provenance (#341), dead code (#357), CI structure (#358), Linux packaging (#359), and
  Windows on ARM testing (#360).

## Next: features

1. **Last.fm scrobbling ([#50]).** Account settings and the internal pipeline are merged. What's
   left is packaging Last.fm application credentials into release builds and an end-to-end check
   against the live service, after the three Last.fm issues above. The
   [Last.fm design](lastfm-scrobbling.md) records the privacy and consent rules.
2. **Download for offline listening ([#11]).** A **Download** action on remote tracks, albums, and
   playlists that saves files into a library folder, where the existing scanner picks them up. The
   plan is in the issue; there is no separate offline cache.
3. **Copy to Device ([#8]).** A **Copy to Device…** action that copies tracks or a playlist to a
   mounted USB drive, SD card, player, or phone in storage mode, with progress and cancel. The plan
   is in the issue.
4. **Drag to the file manager ([#46]).** Drag local tracks out of Tributary to copy the files.
   Dragging remote tracks stays unavailable.
5. **Flathub ([#266]).** Requested; publishing there hasn't been scheduled.
6. **UI refinement ([#29]).** The separator, item-count, and alignment changes are merged; a
   visual and accessibility sign-off remains ([acceptance-p2.3-c.md](acceptance-p2.3-c.md)).

## Not planned, or deliberately limited

- **AirPlay sender.** Tributary doesn't include an AirPlay sender. Route the local output to an
  AirPlay speaker through the operating system, as the README describes. AirPlay 1 rows appear
  only when the installed GStreamer provides `raopsink`; AirPlay 2 receivers are filtered out.
  [airplay-sender-design.md](airplay-sender-design.md) is kept as background research.
- **Removable devices.** No automount or eject, and MTP-only devices aren't supported.
- **Read-only library folders without a marker** can't be added; see "Library Folders" in the
  README.
- **Renames** are tracked only while Tributary is running on Linux or Windows; elsewhere a renamed
  file becomes a new track.
- **Playlist formats.** XSPF is the only import and export format. Apple Music/iTunes XML, Google
  Takeout CSV, and M3U are not read directly, and matching is exact, never fuzzy.
- **Opening several files from the file manager** plays the first valid one. Queueing all of them
  needs a product decision first.
- **Backup and restore** needs a product decision on what a consistent snapshot includes.
- **Apple code signing and notarization** is a distribution decision, not planned feature work.

## Maintenance with a date

- Re-review the retained `paste` and `rkyv` security-advisory exceptions by 2026-12-01 or before
  the next release, whichever comes first, and before enabling `rkyv` serialization.
- Remove the macOS multichannel audio workaround only after an upstream GStreamer fix is in the
  supported runtime and passes testing on affected multichannel hardware.
- Replace the broad GStreamer plugin copy in the Windows and macOS bundles with an allowlist of
  what playback actually needs (see #340).

## Design documents

Current: [Last.fm scrobbling](lastfm-scrobbling.md), [equalizer](equalizer.md),
[source identity and lifecycle](architecture/source-lifecycle.md),
[source-scoped playlists](source-scoped-playlists.md),
[Subsonic playlist sync](subsonic-playlist-sync.md),
[Rhythmbox migration](rhythmbox-migration.md), [ratings](ratings.md),
[playback history](playback-history.md), [native paths](native-path-authority-design.md), and the
[release component policy](release-component-policy.md).

Superseded, kept for background: [offline media](offline-media.md) and the
[AirPlay sender design](airplay-sender-design.md).

[#8]: https://github.com/jm2/tributary/issues/8
[#11]: https://github.com/jm2/tributary/issues/11
[#29]: https://github.com/jm2/tributary/issues/29
[#46]: https://github.com/jm2/tributary/issues/46
[#50]: https://github.com/jm2/tributary/issues/50
[#266]: https://github.com/jm2/tributary/issues/266
