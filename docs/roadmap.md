# Tributary roadmap

Last reviewed: 2026-09-24.

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

## Now: release 0.7.0

The 2026-09-23 review's fixes are merged (merge trains #371 through #416), along with the
Download, Copy to Device, drag-to-file-manager, equalizer, folder browsing, album artwork, and
media-control features. The version is 0.7.0 and is not released yet. Before tagging it, the
features need the real-server and hardware checks listed as V5 in [`task.md`](task.md); CI can't
produce that evidence. After the release, close [#46] and publish the AppStream release notes.

## Next

1. **Last.fm scrobbling ([#50]).** Account settings and the delivery pipeline are merged. What's
   left is packaging Last.fm application credentials into release builds (LF4) and an end-to-end
   check against the live service. Scrobbling tracks from the local library ([#336]) comes
   after that: the library must first record which fields came from tags, so a title taken from
   the file name is never scrobbled. The [Last.fm design](lastfm-scrobbling.md) records the
   privacy and consent rules.
2. **Flathub ([#266]).** Requested; publishing there hasn't been scheduled.
3. **UI refinement ([#29]).** The separator, item-count, and alignment changes are merged; a
   visual and accessibility sign-off remains ([acceptance-p2.3-c.md](acceptance-p2.3-c.md)).

## Waiting for a decision

- **Non-UTF-8 file names ([#258]).** Such files are refused with a notice. Keeping tracks stored
  by older versions (instead of removing them at the next scan) and indexing these files
  losslessly would need a schema change.
- **Android phones over MTP ([#8]).** Copy to Device works with mounted storage only.
- **Opening several files from the file manager** plays the first valid one; queueing all of them
  needs a product decision.

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
- **Playlist formats.** XSPF is the only import and export format; Copy to Device also writes an
  `.m3u8` file next to the copied tracks. Apple Music/iTunes XML, Google Takeout CSV, and M3U are
  not read, and matching is exact, never fuzzy.
- **Backup and restore** needs a product decision on what a consistent snapshot includes.
- **Apple code signing and notarization** is a distribution decision, not planned feature work.

## Maintenance with a date

- Before each release, run `cargo audit` and review any exception in `.cargo/audit.toml`. None
  remain as of 0.7.0.
- Remove the macOS multichannel audio workaround only after an upstream GStreamer fix is in the
  supported runtime and passes testing on affected multichannel hardware.

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
[#29]: https://github.com/jm2/tributary/issues/29
[#46]: https://github.com/jm2/tributary/issues/46
[#50]: https://github.com/jm2/tributary/issues/50
[#258]: https://github.com/jm2/tributary/issues/258
[#266]: https://github.com/jm2/tributary/issues/266
[#336]: https://github.com/jm2/tributary/issues/336
