# Backlog coverage review and expansion proposal

Date: 2026-09-09, America/Indiana/Indianapolis (GitHub snapshot: 2026-09-10 UTC).
Status: adopted into [task.md](task.md). The original review filed ten bugs (#248–#257);
the implementation follow-up also filed the historical native-path limitation as
[#258](https://github.com/jm2/tributary/issues/258). The audit evidence below retains its original
snapshot. No feature implementation, worker assignment, or merge-gate policy was changed.

## Assessment

**The backlog covers the requested features well, but is not comprehensive as an executable
engineering and release plan.** All eight open GitHub feature issues have corresponding records in
`task.md`. The most useful expansion is to reconcile implementation status, make Gas City ownership
and dependencies explicit, add the concrete correctness gaps below, and track integration and
release evidence separately from feature implementation.

Before new issue filing, this review found 43 GitHub issues (eight open), 20 open PRs, and 16 open
`polecat/` PRs. Much of the apparent missing functionality is already being built. Dispatching another
implementation of offline downloads, Android transfer, equalization, artwork, or AirPlay design
would duplicate work.

The source-authority model, playback occurrence ownership, strict migrations, bounded HTTP bodies,
transactional playlist synchronization, and package validation are substantial strengths. The weak
points are at composition boundaries: a path passed to a destructive writer, browser selections
surviving data replacement, parser diagnostics reaching logs, and headless components being counted
as complete before an application or real device exercises them.

## Scope and evidence

Three independent subagents reviewed library/data/authority, playback/networking/Last.fm, and
UI/accessibility. The primary review covered issue reconciliation, PR dependencies, CI, packaging,
and Gas City policy. This was static review plus focused executable probes, not exhaustive formal
verification or a new full cross-platform acceptance run.

- Local checkout: `2a780a6` on `codex/backport-build-run`, initially clean; this is open PR
  [#244](https://github.com/jm2/tributary/pull/244), not the default branch.
- Live `main`: [`ca803ea5`](https://github.com/jm2/tributary/commit/ca803ea5dffa4ce81065fe127a264badb0ed567b).
  Its delta from the common ancestor adds the watcher tests in
  [#222](https://github.com/jm2/tributary/pull/222) and UI refinements in
  [#179](https://github.com/jm2/tributary/pull/179). Those deltas were examined when assessing gaps.
- Read the active backlog, roadmap, relevant design contracts, prior remediation/review documents,
  all open issue bodies/discussions, relevant closed regression reports, and active PR
  descriptions/check summaries. All 43 issue records were retrieved and inventoried.
  Open PR bodies describe intended or reported work;
  they are not independent proof of implementation correctness.
- Queried live main-branch rules, auto-merge settings, and current PR check results. Checks are a
  point-in-time observation and must be re-read before dispatch or merge.
- The live Gas City `city.toml`, rollout manifests, Beads database, and worker state were not
  available in this workspace. Gas City conclusions below use repository policy and GitHub
  bead/PR evidence. Current worker ownership must be confirmed against those external records.

Source line references below refer to the reviewed local checkout unless a pinned main link is
given. The reviewed main delta does not change the identified production tag-write, remote-parser,
or browser-selection paths.

## Reconcile existing records before expanding them

| Area | Current evidence | Proposed disposition |
| --- | --- | --- |
| Completion arithmetic | Snapshot says 15/39; literal count is 16/39 | Correct before expansion |
| Local playlist drag/drop, #46 | [#182](https://github.com/jm2/tributary/pull/182) and [#242](https://github.com/jm2/tributary/pull/242) merged | Audit and close the local-playlist slice; retain export/device-copy work and the broader issue |
| Folder browser, #14 | Model/pane landed in `455e786` | Retain corrective acceptance below |
| UI refinements, #29 | [#179](https://github.com/jm2/tributary/pull/179) merged on September 9 | Reconcile visual/accessibility acceptance and issue state against this revision |
| Chromecast IPv6 | [#174](https://github.com/jm2/tributary/pull/174) merged | Reconcile `task.md:1137` and replace the obsolete IPv4-only claim in `roadmap.md:574` |
| Last.fm, #50 | Policy/migration 20 landed in `e7b07c1` | Keep dormant integration open |
| Watcher harness | Already checked; #222 adds evidence | Reconcile roadmap; no duplicate |
| Historical remediation | 223/226 after exclusions | Surface three release validations |

The arithmetic correction is not permission to close partially accepted features. Each candidate
completion still needs its tests, user documentation, changelog, and complete acceptance contract,
as required by `task.md:13-22`. Do not replace 15/39 with a guessed percentage based on PR titles.

All eight open feature issues are mapped below; there is no uncovered open feature request:

| Issue | Existing record | Remaining treatment |
| --- | --- | --- |
| [#50 Last.fm](https://github.com/jm2/tributary/issues/50) | P2.1 | Finish production consent/account/policy/runtime integration in explicit slices |
| [#49 Equalizer](https://github.com/jm2/tributary/issues/49) | P2.4 | Continue existing design and implementation PRs |
| [#46 Drag/drop](https://github.com/jm2/tributary/issues/46) | P2.2 | Narrow after local-playlist acceptance; track export/copy integration |
| [#39 Browser artwork](https://github.com/jm2/tributary/issues/39) | P2.3 | Continue the existing held redesign |
| [#29 UI refinement](https://github.com/jm2/tributary/issues/29) | P2.3 | Reconcile September 9 merge and acceptance |
| [#14 Folder browsing](https://github.com/jm2/tributary/issues/14) | P2.3 | Fix integration defects, then reconcile completion |
| [#11 Offline cache](https://github.com/jm2/tributary/issues/11) | P3.1 | Preserve the design → contracts → engine → application sequence |
| [#8 Android sync](https://github.com/jm2/tributary/issues/8) | P3.2 | Preserve transfer → MTP → sync sequence; require a real transport and device proof |

## Gas City work already in flight

These are links for continuing existing work, not recommendations to merge the current heads.
Dependencies come from the PR descriptions and must be reconciled with the live bead graph.

| Lane | Existing PRs and beads | Integration/coordination requirement |
| --- | --- | --- |
| Equalizer | [#183](https://github.com/jm2/tributary/pull/183) `tr-sbp` → [#220](https://github.com/jm2/tributary/pull/220) `tr-dkk` | Accept the actual DSP/clipping contract before implementation acceptance |
| Offline | [#181](https://github.com/jm2/tributary/pull/181) `tr-92a` → [#228](https://github.com/jm2/tributary/pull/228) `tr-0ug` → [#231](https://github.com/jm2/tributary/pull/231) `tr-4q8` → [#230](https://github.com/jm2/tributary/pull/230) `tr-8h4` | Reconcile overlapping engine scope; identify owners for persistence, backend download, playback resolution, and window wiring |
| Device sync | [#175](https://github.com/jm2/tributary/pull/175) `tr-0na` → [#178](https://github.com/jm2/tributary/pull/178) `tr-t3i` → [#180](https://github.com/jm2/tributary/pull/180) `tr-bau` | Verify the real MTP adapter, attachment lifecycle, and UI; a transport trait plus test adapter is insufficient |
| Extended drops | [#221](https://github.com/jm2/tributary/pull/221) `tr-4cc` | Design-only today; file export needs the merged #182 source, device copying depends on #175 |
| Removable tag writes | [#232](https://github.com/jm2/tributary/pull/232) `tr-mms4s` | Coordinate its retained authority with local-write work below, without duplicating its patch |
| Artwork | [#171](https://github.com/jm2/tributary/pull/171) `tr-xaj` | Draft redesign explicitly names cancellation, row reuse, retained authority, and cache budgets |
| AirPlay | [#170](https://github.com/jm2/tributary/pull/170) `tr-vp9` | Draft sender-design work; preserve protocol, packaging, and device-acceptance prerequisites |
| MPD ownership | [#173](https://github.com/jm2/tributary/pull/173) `tr-cem` | Draft supervision work; do not interpret detection as an atomic MPD ownership lock |
| Merge policy | [#225](https://github.com/jm2/tributary/pull/225) `tr-96diq`, [#237](https://github.com/jm2/tributary/pull/237) `tr-f875t` | Resolve the policy conflict below before relying on a proposed gate |

The three new Dependabot PRs [#245](https://github.com/jm2/tributary/pull/245),
[#246](https://github.com/jm2/tributary/pull/246), and
[#247](https://github.com/jm2/tributary/pull/247) already belong to the documented repair lane.
Their snapshot has failing checks. In particular #246 has broad compilation/check failures, so
dependency updates are not uniformly mechanical lockfile repairs. PR #244 is the current local
build-helper work and also has unfinished/failing hosted checks; this proposal does not certify it.

### Resolve the merge-policy contradiction

The repository's accepted [refinery policy](refinery-config.md) says operational review is the
out-of-band GLM 5.3 reviewer, repo-owned AI reviewer workflows are retired, and external vendor
statuses are not refinery-review evidence. PRs #225/#237 propose requiring bot/vendor checks and a
new review gate. Those are different policies, not interchangeable descriptions of one rollout.

The live ruleset `17650907` requires seven GitHub Actions contexts: Security Audit, Linux x86_64,
Linux aarch64, macOS aarch64, Windows x86_64, Flatpak Linux, and MSRV. Its strict-base option is
false. Coverage, Desktop Metadata, Windows aarch64, and external analysis/review contexts are not
required by that ruleset. Classic branch protection returned 404; ruleset enforcement is present.
Native auto-merge is enabled and the Dependabot workflow is active. This confirms partial gate
migration: **MSRV is already required**, but a full-matrix policy is not enforced by GitHub alone.

Proposed operational acceptance, within `tr-f875t` and the rollout owner:

1. Read the live city/rollout policy and decide which checks and independent review evidence are
   authoritative. Preserve the accepted out-of-band policy unless an authorized change supersedes it.
2. Reconcile #225/#237 with that decision; retire obsolete requirements rather than accumulating
   reviewer gates that can never complete. Require normal CI results for the exact accepted revision.
3. Align GitHub rules, the native auto-merge precondition, and Gas City's check names/app bindings.
   Prove a deliberately missing/failing required check prevents merge and a complete accepted set
   admits a disposable test PR. Record exact revisions and read back deployed settings.
4. Verify the documented one-hour CI deadline in live `city.toml`; the repository only describes
   this override. Keep pending checks non-green, with bounded failure and repair paths.

No changes to GitHub settings or Gas City were made during this review.

## Proposed new correctness records

The C/Q IDs identify sections of this proposal. The following bug issues were created after the
user requested filing regressions; priorities are recommendations in their bodies, not a change to
Gas City's dispatch order. Each still needs a bead or attachment to an existing owner before
dispatch. Broader design/coverage improvements remain proposals.

| Filed issue | Scope | Proposal mapping |
| --- | --- | --- |
| [#248](https://github.com/jm2/tributary/issues/248) | Tag Save can modify a replacement file | C1 |
| [#249](https://github.com/jm2/tributary/issues/249) | Remote parse diagnostics can expose response values | C2 |
| [#250](https://github.com/jm2/tributary/issues/250) | Search/refresh and visible browser filters disagree | C3 |
| [#251](https://github.com/jm2/tributary/issues/251) | Already-selected folder root/Up cannot navigate | C3; corrective work under #14 |
| [#252](https://github.com/jm2/tributary/issues/252) | Folder derivation loses native Windows directories | Folder completion under #14 |
| [#253](https://github.com/jm2/tributary/issues/253) | Folder entries and root status become stale | Folder completion under #14 |
| [#254](https://github.com/jm2/tributary/issues/254) | Chromecast command ingress is unbounded | C6 |
| [#255](https://github.com/jm2/tributary/issues/255) | Extensionless Cast media is mislabeled as MP3 | C5 |
| [#256](https://github.com/jm2/tributary/issues/256) | Initial scanning can prevent command drain/close | C7 |
| [#257](https://github.com/jm2/tributary/issues/257) | Browser and idle media labels ignore translations | Q5 |
| [#258](https://github.com/jm2/tributary/issues/258) | Historical native-path identity collision | C4; filed on adoption |

Adoption maps the review findings to stable task IDs: C1/C2 become R1/R2; C3 becomes R3/R4 with
folder completion children R5/R6; C4 becomes R11; C5/C6/C7 become R8/R7/R9. Review Q1 maps to task
Q1; review Q2 splits into task Q2/Q3; review Q3 becomes task Q4/Q5/Q6; review Q4 becomes V1–V4;
review Q5 becomes R10. Task Q7 owns the proposed ledger consistency checks. The active task index
has 57 records with 16 complete, retaining all 39 original states and adding no worker claims.

Each issue distinguishes executable reproduction from source-level evidence and states that the
introducing commit has not been established. The original review did not file C4 as a regression;
adoption now tracks that acknowledged historical defect in #258. Quality infrastructure,
backup/restore and existing gate-policy work remain distinct from regression reports.

### C1 — Retain exact local tag-write authority and reject stale edits (P1)

**Observed:** Properties stores a plain path (`src/ui/properties_dialog.rs:34-36`).
`write_tags` opens that pathname, copies and closes it, then unconditionally replaces the current
destination (`src/local/tag_writer.rs:592-648`; `TempFile::persist_to`, line 392).
A file moved/replaced while the dialog is open causes Save to edit a different file. A concurrent
writer between copy and rename can also have its work overwritten.

A standalone probe importing the unchanged production parser/writer captured a selection, moved
the original FLAC away, put a different valid FLAC at its path, and saved: the replacement received
the requested title while the selected original did not. Atomic temporary-file replacement solves
partial writes, not this stale-selection problem.

**Acceptance:** carry the selected track/root/file identity and a content-version precondition
through preview, copy, and commit; reject path/parent/root replacement and concurrent content
changes with a localized conflict result. Define platform guarantees and any residual non-atomic
external-writer race honestly. Test replacement before Save, replacement during copy/commit,
simultaneous edits, permissions, cancellation, and cleanup with deterministic barriers.

**Relationship:** P3.3/#232 is scoped to removable mutation authority. Extend/reuse that design for
local writes with its owner; do not create a competing removable implementation. Prioritize local
data-loss prevention ahead of enabling more write surfaces.

### C2 — Sanitize typed remote-response parse errors (P2)

**Observed:** remote clients format and retain raw `serde_json` errors, for example
`src/subsonic/client.rs:367-370` and `src/plex/client.rs:423-425`; backend warning paths print them,
including `src/subsonic/backend.rs:323,366` and `src/plex/backend.rs:204,222,240`.
An unexpected string in a numeric field is reproduced verbatim by serde's error formatting.
A focused probe with the checkout's serde dependencies confirmed both Display and Debug contain
the complete disposable sentinel value.

Malformed upstream data can therefore expose echoed credentials or private metadata in diagnostics
despite the existing URL stripping and sanitized provider-error messages.

**Acceptance:** use fixed parse categories with safe location information; do not retain a raw
source error that can reintroduce the response content. Cover authentication and catalogue parsers
for Subsonic, Jellyfin, and Plex with sentinel-bearing malformed values; assert absence from Display,
Debug, chained sources, and captured logs. Preserve actionable status/category diagnostics.

### C3 — Make browser navigation and filter state coherent (P2)

**Observed:** `src/ui/browser.rs:381-415` reapplies genre and artist on search changes but omits the
selected album. Selections live in closure-only state (`browser.rs:78-80`), while
`rebuild_browser_data` resets the visible models without clearing that state (`:703-746`).
A production-widget probe confirmed that changing the source can visibly select All but retain
the old artist in subsequent search/album callbacks. Incremental upserts also enter the visible
list without evaluating its filter (`src/ui/window.rs:4036-4059`), before the delayed rebuild resets
the user's search. These are distinct paths to displayed selections disagreeing with results.

Folder navigation listens to `selection_changed` while the model auto-selects or resets row zero
(`src/ui/browser.rs:300-364,502-503`). The first/only root and an already-selected Up row can
consequently receive no navigation callback when selected again.

A GTK probe using the production browser/model code confirmed row zero starts selected and
reselecting it emits zero navigation callbacks. These paths are unchanged by main's #179 delta.

**Acceptance:** make activation independent of whether the row was already selected, define and
apply one genre/artist/album/folder/search state across rebuilds and source switches, and synchronize
visible selections with that state. Exercise first/only root, Up, one-child folders, typing/clearing
search within an album, source switch, library refresh, and restoration after data changes. Include
keyboard activation and accessible state. Assert visible rows and active filters together during
full sync, upsert/delete, and pending search debounce. Attach folder-specific completion to #14
instead of claiming the entire folder feature is missing.

### C4 — Preserve native filesystem path identity (P2, design first)

**Observed:** `src/local/tag_parser.rs:169` stores `path.to_string_lossy()` as `file_path`;
scanner and resolver code subsequently use that text as an actual path. Distinct Unix paths with
invalid UTF-8 can collapse to the same stored text or reconstruct an unplayable replacement path.

A production-parser probe with two distinct native path hints confirmed identical persisted text.
The local APFS volume rejected creating such names, so this is a conversion-level reproduction;
the end-to-end filesystem case requires Linux. The historical tracker acknowledges this limitation,
but there is no active resolution record.

**Acceptance:** decide a reversible platform-tagged path encoding or an explicit supported-input
boundary; keep display strings separate from identity. Cover schema migration, exact lookup,
scanner reconciliation, playlist references, tag writes, and playback. Verify distinct invalid-byte
names on Linux and Unicode/normalization behavior on macOS/Windows. Do not fabricate identity for
already-collided legacy rows; require explicit repair where necessary.

### C5 — Carry validated media type to Chromecast (P2)

**Observed:** protected Subsonic/Jellyfin media routes need not have filename extensions. Cast
publication infers a suffix from a source URL and falls back to `audio/mpeg` for LOAD when no known
extension is available. A FLAC/AAC/other source can therefore be announced as MP3.
The request lacks a representation descriptor (`src/architecture/media.rs:426-433`), ticket
creation infers an extension (`src/audio/cast_http_server.rs:602`), and LOAD derives its type from
the resulting URI (`src/audio/chromecast_output.rs:607-617,2129-2150`).
This is a code-path compatibility finding; no physical Chromecast rejection was reproduced.

**Acceptance:** carry authoritative container/MIME information through the media request/ticket
boundary, including any server transcoding choice; avoid guessing from authenticated endpoint
paths. Define unknown/unsupported handling. Test extensionless MP3, FLAC, AAC, and Ogg requests,
consistent LOAD/HTTP headers, range relays, and representative receivers. Keep credentials absent
from receiver URLs. This is separate from the completed IPv6 publication record.

### C6 — Bound Chromecast command admission (P2)

**Observed:** `src/audio/chromecast_output.rs:774` uses unbounded `mpsc::channel`; every command is
sent at `:1860-1862`, including seek/volume changes (`:2080-2086`). A serial worker may spend up to
the transport deadline on one request. A slow but responding receiver lets transient user intents
accumulate and replay after they are useful. Epoch checks suppress stale effects after dequeue;
they do not bound pending allocation or compact same-generation commands. MPD already has a
bounded admission design (`src/audio/mpd_output.rs:172`); its tests do not cover Chromecast.

**Acceptance:** define a finite, nonblocking budget and explicit saturation behavior; coalesce only
safe transient seeks/volume changes, promptly discard obsolete generations, and reserve Stop and
Shutdown admission. Hold a fake receiver command, flood well beyond capacity, assert a fixed queue
bound and final user intent, then prove stop, output replacement, and shutdown remain admissible.
Keep this separate from IPv6 publication and the existing MPD supervision PR.

### C7 — Make scan cancellation and command draining responsive (P2)

**Observed:** `LibraryEngine::run` awaits the initial scan (`src/local/engine.rs:518`) before
servicing commands (`:538`). Traversal, parsing, and blocking root/file probes are awaited without
a scan cancellation token (`:3622-3627,3952-3955,1322-1327`). Window close enqueues Flush, disables
the window, and waits for acknowledgement without a deadline (`src/ui/window.rs:1983-2043`).
Command submission is also unbounded (`src/ui/library_commands.rs:30-32`). A long initial scan
delays close and admitted edits/history; a stalled mounted filesystem can hold this path
indefinitely. This is a concrete control-flow risk, not a reproduced live-mount hang, and is
independent of the access-event feedback loop fixed for #166/#204 and the harness in #222.

**Acceptance:** define startup/close/command-admission budgets; allow shutdown to stop admitting
scan writes, settle already-admitted durable commands, and complete within the supported I/O
contract. Use deterministic held traversal/parser fixtures and test cancellation during overflow
reconciliation without treating incomplete scans as deletion authority. A timeout around
`spawn_blocking` cannot stop an in-progress kernel call: explicitly design worker isolation or
document the residual limit. Coordinate ownership changes with Last.fm's existing shutdown owner.

## Expand quality and release coverage

### Q1 — Run real GTK interaction contracts in CI (P2)

Main's #179 consolidates widget tests correctly, but its
[`widget_test_session`](https://github.com/jm2/tributary/blob/ca803ea5dffa4ce81065fe127a264badb0ed567b/src/ui/mod.rs#L59)
skips without DISPLAY/WAYLAND_DISPLAY and excludes macOS. Current Linux CI installs no display
server and does not launch one. A green test suite therefore does not establish these widget paths.

Add a bounded Linux display-backed job that fails if the expected interaction tests skip; preserve
GTK thread affinity in one process. Exercise navigation/search, selection restoration, drag/drop,
settings persistence, and close barriers through production widgets. Add a small recorded manual
matrix for keyboard-only operation, screen-reader labels, high contrast, scaling, and representative
long translations. Native macOS/Windows smoke evidence remains a separate platform obligation.

### Q2 — Audit both dependency lockfiles and broaden parser fuzz coverage (P2/P3)

CI runs `cargo audit` only at the repository root (`.github/workflows/ci.yml:48-51`). The independent
fuzz workspace receives locked fmt/clippy checks but no explicit audit of `fuzz/Cargo.lock`.
`docs/dependency-updates.md` correctly says its transitive dependencies can legitimately differ.
Root-lock synchronization is therefore not a security audit of that graph.

First add an explicit audit of both graphs with independently justified advisory exceptions and
fixtures proving the fuzz lock is actually selected. This is a coverage gap, not evidence of a
currently exploitable dependency. Then extend the existing DMAP-only fuzz surface in bounded slices
to XSPF/Rhythmbox XML, strict Last.fm response parsing, and URL/ticket/range parsing. Use the
production parser code with corpus and resource limits; do not expose credentials or live servers.

### Q3 — Establish measurable large-library and resource budgets (P2)

Track end-to-end responsiveness rather than relying on a global line-coverage percentage. The
scanner, catalogue publication, browser rebuild, artwork decoding, and shutdown/drain boundaries
need acceptance budgets exercised together. Existing body-size bounds and component tests are
useful but do not imply a bound on total catalogue rows, decoded images, or time to accept a command.

Two concrete resource surfaces deserve explicit design children:

- The relay starts blocking authority/body workers per valid request
  (`src/audio/cast_http_server.rs:952-960,1027-1049`) without a process/ticket concurrency budget.
  Its two-chunk response channel bounds one response, not all slow consumers. Define admission
  permits held through response completion and test many parallel ranges, stalled readers, and
  capacity recovery. This concerns valid ticket holders, not a claim of an unauthenticated relay.
- Jellyfin/Plex accumulate many pages in memory (`src/jellyfin/backend.rs:334-361`,
  `src/plex/backend.rs:352-392`); page caps can warn then return a successful partial result.
  Define total rows/bytes/work and repeated-page detection, plus visible partialness or rejection
  semantics so a limit is not confused with authoritative catalogue absence.

Start with fixed 10k/100k-track fixtures and delayed filesystem/backend adapters; record time to
interactive, search/rebuild latency, peak retained rows/bytes, cancellation settlement, and command
admission during initial scan. Agree pass/fail budgets on a documented runner. File implementation
slices only where measurements or concrete code findings show the budget is not met. Avoid a
general rewrite of large modules solely because their line counts are high.

### Q4 — Make release validation a visible owned lane (P2)

Retain the three existing archive validations: real removable hardware, installed Flatpak
portal/custom-root/USB behavior, and packaged Windows DAAP/Subsonic playback. Give each an owner,
artifact SHA/version, exact environment, test steps, and pass/fail evidence in the current release
queue. Add corresponding macOS packaged remote-playback evidence following #243; its bundle-only
decode/proxy-policy probe is strong evidence but does not test every real backend or device.

Track Apple signing/notarization as an explicit distribution decision outside the feature
percentage, as already intended. Record release support/known limits and package acceptance per
platform. Do not reopen the completed release-tag dry-run or reimplement existing icon, component,
checksum, and protected-stream probes.

### Q5 — Localize existing browser and idle-media UI (P3)

Browser headings and All rows are English literals (`src/ui/browser.rs:129-132,624,659,688`)
despite existing translated browser keys in the catalogs. Folder status messages and desktop
integration's idle title are also literal English (`browser.rs:795-825`,
`src/desktop_integration/mod.rs:119,165`). New feature localization requirements do not repair
this existing chrome.

Use the existing keys and add semantic status keys where needed, with catalog parity and a
production-widget check in a non-English locale. Keep user media/server names intact. Never use
translated labels as row identity. Folder-only strings can travel with #14's corrective slice;
the rest belongs to one bounded localization completion record.

### Optional product decision — Library backup and restore

The database now holds user-created ratings, history, playlists, links, and import receipts as
well as Last.fm queue state. Strict migration failure correctly stops startup
(`src/db/connection.rs:60-61`, `src/db/migration/mod.rs:69-77`), but there is no active backup/restore
contract. Consider a scoped design for consistent snapshots, restore compatibility/integrity,
retention, recovery UI, and explicit privacy treatment of queued scrobbles. This is a proposed
product decision, not a detected migration defect or permission to copy a live SQLite file.

## Refine existing epics instead of adding duplicate epics

- **Last.fm:** retain #50 and P2.1. Give remaining slices individual acceptance records for local
  and remote attribution, shared live policy at queue capture/dispatch, consent/browser/account
  controls, disconnect/recovery/reauthorization, package credentials, and end-to-end drain/privacy
  evidence. Durable policy storage is already implemented. Explicitly resolve the application's
  one-shot activation versus successor policy/account generations, including same-account
  reauthorization and different-account purge/install. One owner must maintain the lifecycle
  transaction across these slices; credentials remain an explicit external release dependency.
- **Offline:** retain #11 and its four PRs. Require one integrated proof from an authorized remote
  selection through durable job/restart, publication, offline catalogue lookup and playback, plus
  cancel/logout/delete/quota outcomes. #230 explicitly defers window wiring, and #228 explicitly
  split its contracts from the engine. Assign unowned glue as children of this epic after comparing
  current heads. Neither a contracts PR nor a rendered unattached panel completes offline playback.
- **Android:** retain #8 and its three PRs. #178 describes a transport interface and an in-memory
  test implementation; that description does not prove working MTP device I/O. Require a selected
  native transport, capability detection/packaging, actual device enumeration/transfer, detach and
  permission recovery, and application access to the planner. A mounted filesystem fake cannot be
  the final evidence for pathless Android storage.
- **Artwork:** keep the request-local cancellation, recycled-row safety, retained read authority,
  and decoded-memory budget under the existing #171 redesign. They are already recorded there.
- **File export/device drops:** #221 is explicitly design-only; add application implementation
  children under #46 after the relevant authority/transfer prerequisites are accepted.
- **Folder browsing:** keep #14 open until C3 and its remaining original acceptance are met.
  Use native path components instead of splitting a Windows `PathBuf` string only on `/`
  (`src/ui/folder_browser.rs:307-330`); feed authoritative root identity instead of always passing
  `None` to `BrowsableRoot::from_configured` (`src/ui/window.rs:3894-3915`); update folder entries
  after incremental add/delete (`window.rs:4099,4176`). Use a typed Up row rather than the display
  label `…`, which can be a real directory name (`browser.rs:333,756`). Test nested Windows paths,
  root replacement/rename, new/removed subdirectories, and navigation state through these changes.

## Proposed task and issue structure

Keep `task.md` as a compact execution index and move its long implementation narrative into the
existing design documents or a historical log. The 1,257-line file devotes hundreds of lines to one
still-open Last.fm record, while broad downstream epics receive a few lines. This makes selecting
an independently complete next assignment unnecessarily difficult.

Give each executable record these fields:

```text
Stable ID and title
Kind: implementation | integration | validation | maintenance | operator rollout
Priority and state: proposed | ready | active | blocked | review | merged | validated
GitHub issue / Gas City bead / owner / current PR
Dependencies and shared files or authority/schema boundaries
Acceptance behavior, tests, and required environment
Evidence: accepted commit, CI run, product proof, docs/changelog
Known blocker or explicit non-goal
```

Use one source for state transitions, with task.md rendering/linking that state. Add a lightweight
consistency check for literal completion counts, unique IDs, broken internal links, missing
issue/bead/PR mappings on active records, and merged-but-unreconciled records. A merged child must
not automatically complete its parent acceptance contract. Keep implementation percentage separate
from release validation and from effort estimates.

Before dispatch, Gas City should reconcile current bead ownership and dependencies with the PR
map above; preserve explicit holds and confirm the head that a review actually accepted. Where a
record has been split, update its acceptance and dependency graph together. Existing failures or
review holds should route to the existing Repairer/owner instead of producing duplicate workers.
Cross-cutting edits to the source registry, lifecycle owners, DB migrations, browser, and window
composition need a declared integration owner and isolated branches.

## Suggested sequence

1. Reconcile status/counts and the live Gas City gate policy. Attach existing PRs and owners;
   preserve existing holds. This can run independently of the corrective code work.
2. Address C1 local write identity and C2 parse-error privacy. Reuse existing authority and
   diagnostic boundaries, with focused regression tests before widening features.
3. Fix C3 browser interactions and add Q1 display-backed verification. Reconcile #14/#29/#46
   acceptance on the integrated result. C5 media types, C6 Cast admission, and C7 scan scheduling
   are independent corrective slices with declared ownership of their lifecycle boundaries.
4. Land Q2's separate fuzz-lock audit; establish Q3 measurements. Design C4's path representation
   without blocking unrelated Last.fm or output work.
5. Continue the current Gas City chains in dependency order, adding only missing integration
   children. Complete Q4's environment proofs before claiming those release capabilities.

## Validation performed and limits

- `python3 scripts/test_build_helpers.py`: 6 passed.
- `python3 scripts/test_dependency_update_policy.py`: 30 passed.
- `python3 scripts/sync_rust_toolchain.py --check`: synchronized.
- `cargo test --locked --offline --test packaging_metadata`: 28 passed.
- Focused production-code probes reproduced stale tag-write target selection and lossy native
  path conversion; the latter was not an end-to-end invalid-filename test on this APFS host.
- Focused serde probe reproduced content-bearing Display/Debug parse diagnostics.
- Focused GTK probe reproduced the already-selected first-folder-row navigation problem.
  The same probe confirmed stale artist state after a visible selection reset on source rebuild.

These checks do not certify the whole application or the open PRs. Full debug/release application
tests, live Last.fm, real Chromecast/AirPlay/MPD interoperability, installed Flatpak, Android/MTP,
and the complete native package matrix were not rerun for this proposal. New correctness findings
are distinguished above from compatibility inferences and proposed quality coverage.
