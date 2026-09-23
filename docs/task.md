# Tributary backlog index

Last reconciled: 2026-09-23 (main `76385d0f`).

**GitHub issues are the source of truth.** Each issue holds its own scope, acceptance criteria,
discussion, and state. This file only maps the stable IDs used in older docs, commits, and PR
titles (R1, Q4, P2.3-B, …) to their issues, and lists evidence that CI cannot produce. It has no
completion counters.

- Implementing PRs say `Closes #N` or `Refs #N`. Closing the issue is what marks the work done.
- Edit this file only when an ID's issue or state changes (open → done or dropped). Don't copy
  acceptance criteria here.
- [`roadmap.md`](roadmap.md) describes product direction. Before 2026-09-23 this file was a much
  longer execution index; that text is in git history (`git show 76385d0f:docs/task.md`), and
  the earlier delivery log is in
  [task-implementation-history-2026-09-09.md](task-implementation-history-2026-09-09.md). The
  completed 2026-07 remediation is in [task-remediation-2026-07.md](task-remediation-2026-07.md).

Issues filed by the 2026-09-23 review (#312–#362) have no stable IDs; they are tracked only in
GitHub.

## Open

| ID | Issue | Scope | State |
| --- | --- | --- | --- |
| R1 | [#248](https://github.com/jm2/tributary/issues/248) | Tag saves can modify a replacement file | #284 merged; confirm the remaining acceptance on the issue |
| R6 | [#253](https://github.com/jm2/tributary/issues/253) | Folder entries and root status stay stale after library changes | Open |
| R10 | [#257](https://github.com/jm2/tributary/issues/257) | Browser headings and idle metadata ignore translations | Open |
| R11 | [#258](https://github.com/jm2/tributary/issues/258) | Lossless or explicitly refused native local paths | Design merged (#283, [native-path-authority-design.md](native-path-authority-design.md)); implementation open |
| Q5 | [#299](https://github.com/jm2/tributary/issues/299) | Media-relay resource limits | Open, design first |
| Q6 | [#300](https://github.com/jm2/tributary/issues/300) | Aggregate remote catalogue limits and partialness | Open, design first |
| P2.1-B | [#50](https://github.com/jm2/tributary/issues/50) | Last.fm scrobbling | LF1 (#289), LF2 (#305), LF3 (#308) merged; LF4 (packaged credentials, live acceptance) and #335–#337 open |
| P2.2-B | [#46](https://github.com/jm2/tributary/issues/46) | Remaining drag-and-drop targets | File-manager export next, as a small PR under #46; remote-row drops stay unavailable; device copy follows #8 |
| P2.3-A | [#14](https://github.com/jm2/tributary/issues/14) | Folder browsing | Shipped in `455e786`; R3–R5 done; R6 and R10 remain |
| P2.3-C | [#29](https://github.com/jm2/tributary/issues/29) | Separator, count-opacity, and alignment refinements | Merged (#179); visual and accessibility sign-off per [acceptance-p2.3-c.md](acceptance-p2.3-c.md) pending |
| P2.4-F | — | Chromecast IPv6 media | Merged (#174, #278); a check against a real IPv6 receiver is pending |
| P3.1 | [#11](https://github.com/jm2/tributary/issues/11) | Offline downloads | Replaced by a simple Download action into a library folder (plan on #11) |
| P3.2 | [#8](https://github.com/jm2/tributary/issues/8) | Copy to devices | Replaced by a Copy to Device… action (plan on #8) |
| P3.3-B | — | Queue every file from a multi-file OS open | Blocked on a product decision |
| P3.4-B | — | Re-review the `paste` and `rkyv` advisory exceptions | Due by 2026-12-01 or the next release |
| P3.4-C | — | Remove the macOS channel-cap workaround | Blocked on an upstream GStreamer fix and multichannel hardware tests |

## Done

| ID | Issue | Merged work |
| --- | --- | --- |
| R2 | [#249](https://github.com/jm2/tributary/issues/249) | #272 |
| R3 | [#250](https://github.com/jm2/tributary/issues/250) | #298; full-sync view preservation in #380 |
| R4 | [#251](https://github.com/jm2/tributary/issues/251) | #310 |
| R5 | [#252](https://github.com/jm2/tributary/issues/252) | #297 |
| R7 | [#254](https://github.com/jm2/tributary/issues/254) | #287 |
| R8 | [#255](https://github.com/jm2/tributary/issues/255) | #295 (a real-receiver check is still useful) |
| R9 | [#256](https://github.com/jm2/tributary/issues/256) | #286, #376 |
| Q1 | [#274](https://github.com/jm2/tributary/issues/274) | #277 |
| Q2 | [#279](https://github.com/jm2/tributary/issues/279) | #282; superseded by the single workspace lockfile (#365) |
| Q3 | [#301](https://github.com/jm2/tributary/issues/301) | #304 |
| Q4 | [#275](https://github.com/jm2/tributary/issues/275) | #285, #291, #306 |
| Q7 | [#276](https://github.com/jm2/tributary/issues/276) | #280; the checker was retired on 2026-09-23 along with the counters it checked |
| P2.2-A | [#46](https://github.com/jm2/tributary/issues/46) | Drops onto local playlists: #182, #242, #290 |
| P2.3-B | [#39](https://github.com/jm2/tributary/issues/39) | Album artwork in the browser: #171 |
| P2.4-A, P2.4-B | [#49](https://github.com/jm2/tributary/issues/49) | Equalizer design #183; implementation #368 (replaced #220) |
| P2.4-C | — | AirPlay sender design #170; the sender itself (P2.4-D/E) was dropped, see below |
| P2.4-G | — | Optional MPD supervision: #173, with fixes in #375 |
| P1.x, P2.1-A, P3.3-A, P3.4-A, P3.4-D | — | Completed before 2026-09-09; see the history file |

## Closed on 2026-09-23 in favour of simpler plans

- **Offline stack** (P3.1-B/C): PRs #228, #231, #230. They were never wired into the app. See the
  plan on #11; [offline-media.md](offline-media.md) is kept as background.
- **Device sync** (P3.2-A/B/C): PRs #175, #178, #180. No user-facing action and no real MTP
  transport. See the plan on #8.
- **AirPlay OwnTone sender** (P2.4-D/E): PR #270. It needed a hand-configured second OwnTone
  instance. Its receiver-discovery and output-row fixes were kept in #363. AirPlay speakers are
  reached through the operating system (see the README).
- **Bot review gate** (part of O1): PR #237, which depended on GitHub Apps and secrets that don't
  exist.
- **Drop-policy design** (P2.2-B): PR #221. File-manager export goes straight into a small PR
  under #46.

## Operator and release evidence

These need a person, a live service, or real hardware; CI can't produce them. Record the date,
the exact commit or artifact, the environment, and the result on the linked issue or release PR.

- **O1 — Merge gate policy.** Required checks and admin bypass are tracked in #339.
- **O2 — Live CI timeout and check names.** Done (#288).
- **V1 — Removable media.** Browse, play, and detach/reconnect on real removable hardware.
- **V2 — Flatpak.** Portal, custom-root, and USB permissions in an installed Flatpak.
- **V3 — Windows package.** Packaged playback and reconnect against real DAAP and Subsonic
  servers.
- **V4 — macOS package.** Protected playback and output-device switching (including unplug and
  replug) in a bundle without Homebrew.
