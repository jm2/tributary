# Tributary active implementation backlog

Last audited: 2026-09-09 (America/Indiana/Indianapolis; GitHub readback 2026-09-10 UTC).

This is the execution index for implementation, corrective work, and engineering follow-ups.
GitHub is authoritative for issue/PR state; Gas City's live bead ledger owns worker assignments.
The [review proposal](backlog-review-proposal-2026-09-09.md) records the evidence. Detailed prior
acceptance and delivery history are preserved in
[the September snapshot](task-implementation-history-2026-09-09.md); the earlier remediation
remains in [task-remediation-2026-07.md](task-remediation-2026-07.md).

## How to use this file

- Each top-level checkbox is one countable record with a stable ID. Nested slices and the
  operator/release tables are obligations, not additional implementation boxes.
- Keep a record open until its complete behavior, tests, user documentation, and changelog are
  merged. A merged partial PR does not complete its parent. Linked design contracts and archived
  acceptance criteria still govern existing records; these summaries do not weaken their scope.
- States are Ready, In flight, Blocked, Merged (acceptance pending), or Complete. A checked record
  is Complete. New R/Q records are unassigned until linked to a live owner and bead; Ready describes
  scoped work, not a claimed assignment.
- Before dispatch, record issue, bead, owner, PR/head, dependencies, and accepted design in the live
  ledger. Create/refine an issue for a Q record before activation. Use stable IDs instead of task.md
  line numbers. Preserve existing worker claims and explicit review holds.
- Protocol, schema, authority, privacy, and cross-output changes require an accepted design or
  refined issue before implementation. Declare an integration owner for shared lifecycle, root,
  migration, browser, and window edits; review the exact head before admitting dependent work.
- Update this index, roadmap, and issue state in the implementing PR. Keep literal counters
  synchronized, count release/operator evidence separately, and never interpret counts as effort.

Current status: **16/57 (28.1%)** implementation records complete: the retained baseline is
**16/39**, with **0/11** new corrective and **0/7** engineering records complete. The earlier
15/39 headline was an arithmetic error. This expansion closes no feature record. The archived
remediation remains **223/226 (98.7%)** after its documented exclusions; its three open environment
validations are surfaced below. Eight feature issues and eleven filed bugs are open at this audit
snapshot; GitHub may change after publication.

## Current focus

Schedule R1 local tag-write safety first and R2 diagnostic privacy next. R3–R6 browser corrections,
R7–R9 output/scan resilience, and Q1 real GTK verification are independently useful when shared-file
owners can coordinate. R11 is design first, not a prerequisite to unrelated playback work. These
priorities do not revoke current worker claims.

Last.fm P2.1-B remains the ongoing listening integration. Production is Dormant, but durable policy
generations and migration 20 already landed in `e7b07c1`; do not rebuild them. Continue the remaining
lifecycle/account/application slices under one owner while existing Gas City work follows its graph.

Unchecked features with merged code include local playlist drag/drop (#182/#242), folder browsing
(`455e786`), presentation refinements (#179), and Chromecast IPv6 (#174). They remain Merged
(acceptance pending) until full behavior and documentation are reconciled. Folder browsing has
explicit corrective children; do not dispatch another implementation of the original feature.

## Gas City in-flight map

This September 9 snapshot does not replace live worker state. Branches use `polecat/<bead>`.
The order describes required acceptance even where GitHub bases currently target main; reconcile
overlapping patches before merge. The
[proposal's PR map](backlog-review-proposal-2026-09-09.md#gas-city-work-already-in-flight)
links every PR.

| Lane | Existing PRs / beads | Dependency or hold |
| --- | --- | --- |
| Equalizer | #183 `tr-sbp`; #220 `tr-dkk` | Design then implementation |
| Offline design | #181 `tr-92a` | Draft; precedes contracts |
| Offline contracts | #228 `tr-0ug` | After design; precedes engine |
| Offline engine | #231 `tr-4q8` | After contracts; share persistence/resolver |
| Offline application | #230 `tr-8h4` | After engine; complete window wiring |
| Device sync | #175 `tr-0na`; #178 `tr-t3i`; #180 `tr-bau` | Transfer, real MTP, sync integration |
| Extended drops | #221 `tr-4cc` | Design; export follows #182, device copy follows #175 |
| Removable writes | #232 `tr-mms4s` | Coordinate common authority with R1 |
| Artwork | #171 `tr-xaj` | Draft cancellation/authority/cache redesign |
| AirPlay | #170 `tr-vp9` | Draft design; no sender before acceptance |
| MPD supervision | #173 `tr-cem` | Draft; detection is not an atomic ownership lock |
| Merge policy | #225 `tr-96diq`; #237 `tr-f875t` | Resolve O1 before relying on proposed gates |

Dependabot #245–#247 belong to the [existing repair lane](dependency-updates.md). Diagnose the
actual failing check rather than assuming a generic lock repair. Build-helper #244 is separate work.

## R — Corrective records

Introducing commits are not established unless the issue says otherwise. Each issue distinguishes
executable reproduction, static findings, and physical-platform verification still due. The issue's
full acceptance is binding; the checklist below indexes it. No new record has a claimed worker yet.

- [ ] **R1 — Tag saves can modify a replacement file instead of the selected track**
  (P1; Ready; [#248](https://github.com/jm2/tributary/issues/248)).
  Acceptance: Retain the selected track/root/file identity and a meaningful content revision through
  selection, staged write, and commit. Reject changed targets/ancestors or conflicting edits with a
  localized conflict result; preserve the competing file/update. Add deterministic tests for
  replacement before Save, replacement/concurrent edit during copy/commit, root changes,
  cancellation, permissions, and temp cleanup. State residual platform I/O races honestly.
  Coordinate exact mutation authority with #232 / tr-mms4s; the existing local editor is the
  affected surface.

- [ ] **R2 — Remote JSON parse failures can leak response values into diagnostics**
  (P2; Ready; [#249](https://github.com/jm2/tributary/issues/249)).
  Acceptance: Return fixed parse categories and safe line/column data without retaining a
  content-bearing source error. Add production HTTP fixtures for auth and catalogue parsers with
  sentinel-bearing wrong-type values, including large strings. Assert Display, Debug, error chains,
  captured tracing and UI projections omit response content; preserve useful category/auth status
  behavior.
  Preserve Last.fm’s existing strict parser; this concerns other remote auth/catalogue paths.

- [ ] **R3 — Browser search and refresh leave visible selections inconsistent with filters**
  (P2; Ready; [#250](https://github.com/jm2/tributary/issues/250)).
  Acceptance: Use explicit shared genre/artist/album/folder/search state and one composition rule.
  Define reset vs preservation for source replacement versus same-source refresh; displayed
  selections and evaluated results must agree. Add production-widget tests for album +
  typing/clearing, source A→B then album/search, upsert/delete under filters, full sync, and a
  pending search debounce during source change.
  Coordinate browser state with R4–R6 and the current #171 artwork owner.

- [ ] **R4 — Folder browser cannot navigate an already selected root or Up row**
  (P2; Ready; [#251](https://github.com/jm2/tributary/issues/251)).
  Acceptance: Navigate through explicit row activation independent of selection changes. Give root,
  directory, Up and status rows typed identities rather than interpreting labels. Test sole/first
  root, empty leaf, repeated Up, legitimate directory named …, refresh, and pointer/keyboard parity
  through production widgets.
  Corrective child of #14 / P2.3-A; use typed row identity and pointer/keyboard activation.

- [ ] **R5 — Folder browsing splits only on slashes and loses Windows directories**
  (P2; Ready; [#252](https://github.com/jm2/tributary/issues/252)).
  Acceptance: Derive and compare navigation paths with native Path components or a deliberately
  defined portable representation. Add native Windows fixtures for multiple levels, drive roots,
  spaces, Unicode, siblings with common prefixes, and folder filtering. Keep the current
  no-escape/root containment contract and test equivalent behavior on Unix.
  Corrective child of #14 / P2.3-A; require native Windows coverage.

- [ ] **R6 — Folder entries and root status stay stale after library changes**
  (P2; Ready; [#253](https://github.com/jm2/tributary/issues/253)).
  Acceptance: Project authoritative library-root identity and availability into the browser instead
  of reconstructing authority from a display pathname. Update visible folder entries on accepted
  add/delete/rename/full-sync events. Preserve a valid current folder or move to an explicit
  localized unavailable/fallback state when it disappears. Test root
  rename/replacement/offline/reconnect, new and removed subdirectories, nested roots, source
  changes, and stale event rejection.
  Corrective child of #14 / P2.3-A; reuse the existing root/registry lifecycle owner.

- [ ] **R7 — Chromecast control queue grows without bound under slow receivers**
  (P2; Ready; [#254](https://github.com/jm2/tributary/issues/254)).
  Acceptance: Define finite nonblocking admission with explicit overload behavior and safe transient
  seek/volume coalescing. Purge obsolete epochs promptly and reserve Stop/Shutdown admission without
  waiting on the receiver. Hold a command in the existing fake Cast transport, flood beyond
  capacity, assert retained bounds and final intent, and prove stop/replacement/shutdown can still
  settle.
  Independent of completed IPv6 and #173; MPD saturation tests do not cover Cast.

- [ ] **R8 — Chromecast labels extensionless protected media as audio/mpeg**
  (P2; Ready; [#255](https://github.com/jm2/tributary/issues/255)).
  Acceptance: Carry a validated non-secret container/MIME and live/buffered descriptor through media
  resolution, ticket creation, and LOAD. Define authority for this descriptor under server
  transcoding and make unknown/unsupported behavior explicit. Assert actual outbound LOAD plus HTTP
  bytes/headers for extensionless MP3/FLAC/Ogg/AAC and transcoded media; retain range and credential
  isolation. Record representative real-receiver acceptance separately.
  Physical receiver acceptance remains required; do not relabel this as missing IPv6.

- [ ] **R9 — Initial scan can indefinitely delay command draining and window close**
  (P2; Ready; [#256](https://github.com/jm2/tributary/issues/256)).
  Acceptance: Define scan/close/command latency and admission budgets, with a reserved
  shutdown/drain path. Stop admitting scan mutations on cancellation, settle already admitted
  durable work, and preserve incomplete-scan/no-deletion authority semantics. Use held
  traversal/parser fixtures to prove close/cancel and admitted commands can settle safely, including
  overflow reconciliation and restart. Explicitly handle the fact that timing out spawn_blocking
  does not cancel an in-progress kernel call; use an appropriate worker/isolation contract rather
  than dropping mutation futures.
  Coordinate Last.fm shutdown ownership; preserve incomplete-scan/no-deletion authority.

- [ ] **R10 — Browser headings and idle media metadata ignore existing translations**
  (P3; Ready; [#257](https://github.com/jm2/tributary/issues/257)).
  Acceptance: Use the existing browser keys and add semantic folder/idle-state keys consistently
  across supported catalogs. Verify actual production labels in a non-English locale and catalog key
  parity. Keep user media/server names intact and never use translated display strings as semantic
  row identity.
  Folder-only strings can land with R4–R6; preserve user metadata and semantic row identity.

- [ ] **R11 — Lossless or explicitly refused native local paths**
  (P2; Ready (design first); [#258](https://github.com/jm2/tributary/issues/258)).
  Acceptance: Decide a versioned reversible native-path representation or an explicit
  unsupported-input boundary; separate display text from authoritative identity. Preserve existing
  track IDs, history, ratings and playlist references only where exact identity is provable.
  Quarantine ambiguous legacy rows rather than guessing. Cover scanner lookup/reconciliation, schema
  migration, playback, tag writes and import/export under the accepted contract. Add Linux fixtures
  for distinct invalid-byte filenames and literal replacement-character collisions, plus
  Unicode/normalization and rename cases on supported platforms. Until lossless support exists,
  define safe rejection/diagnostics instead of silently storing false playback authority.
  Acknowledged historical limitation, now actively tracked; coordinate R1 without blocking unrelated
  Last.fm work.

## P1 — Correctness and shared feature foundations

### P1.1 — Harden and document existing shuffled playback history

- [x] **P1.1-A** — Bound, specify, and fully regress the existing occurrence-aware shuffle history
  ([#132](https://github.com/jm2/tributary/pull/132)).

  Complete. Detailed acceptance and PR #132 evidence remain in the historical snapshot.

### P1.2 — Make unsupported remote playlist actions honest

- [x] **P1.2-A** — When Add to Playlist cannot accept a remote row, show a localized, user-visible
  result instead of only logging that the row was skipped
  ([#47](https://github.com/jm2/tributary/issues/47);
  [#133](https://github.com/jm2/tributary/pull/133)).

### P1.3 — Record trustworthy local playback history

- [x] **P1.3-A** — Define and migrate the durable playback-history contract: counted-play threshold,
  `last_played`, repeat/seek/restart semantics, clock representation, and legacy-row behavior
  ([contract](playback-history.md); [#134](https://github.com/jm2/tributary/pull/134)).

  Complete; contract: [playback-history.md](playback-history.md); delivery PR #135.

- [x] **P1.3-B** — Persist play-count and last-played updates from authoritative playback events
  exactly once, without counting rejected loads, stale generations, or retries, and refresh affected
  UI state ([#135](https://github.com/jm2/tributary/pull/135);
  [#136](https://github.com/jm2/tributary/pull/136)).

  Complete; authoritative occurrence/history delivery PR #136.

- [x] **P1.3-C** — Make Recently Played and Top 25 reflect the new history contract
  deterministically, including live refresh, ordering, empty-state, migration, and regression
  coverage ([#137](https://github.com/jm2/tributary/pull/137)).

  Complete; deterministic history playlists delivery PR #137.

### P1.4 — Add ratings as a real library field

- [x] **P1.4-A** — Decide rating ownership and capability semantics, then add the database
  migration, model, backend propagation, import/export representation, and safe legacy defaults
  ([#37](https://github.com/jm2/tributary/issues/37),
  [#138](https://github.com/jm2/tributary/pull/138)).

  Complete; [ratings.md](ratings.md), PR #138.

- [x] **P1.4-B** — Add accessible editing, display, sorting, and smart-playlist rules, with explicit
  behavior for read-only or rating-incapable sources
  ([#139](https://github.com/jm2/tributary/pull/139)).

  Complete; accessible rating/smart-rule delivery PR #139.

### P1.5 — Persist source-scoped playlists

- [x] **P1.5-S** — Design and migrate regular playlist entries from local track foreign keys to
  stable source-scoped `(SourceId, TrackId)` identity, with deterministic local migration, ordering,
  duplicate-occurrence, unavailable-source, deletion, and rollback behavior
  ([contract](source-scoped-playlists.md); [#47](https://github.com/jm2/tributary/issues/47);
  [#140](https://github.com/jm2/tributary/pull/140)).

  Complete; [source-scoped-playlists.md](source-scoped-playlists.md), PR #140.

- [x] **P1.5-A** — **Record A — Live catalogue authority:** establish the live-registry and
  accepted-catalogue authority foundation for source-scoped regular-playlist entries. Capability
  must default to unsupported and opt in only authenticated Subsonic, Jellyfin, Plex, and DAAP
  adapters. Ordered lookup must accept only the exact current source session, catalogue generation,
  and native-track identities, returning no locator, credential, lease, or route. Its closed result
  may carry the non-secret session epoch and catalogue generation transiently; neither becomes
  playlist storage. Guarded media resolution remains a separate at-use operation with retained
  private authority ([#141](https://github.com/jm2/tributary/pull/141)).

  Complete; live catalogue authority PR #141.

- [x] **P1.5-B** — **Record B — Mixed-source UI integration:** integrate the registry authority
  foundation into regular-playlist Add, Remove, rendering, and Play behavior, with explicit
  disconnected/missing states, source retirement, stale-epoch rejection, occurrence ordering,
  duplicates, and all-or-none multi-selection tests
  ([#142](https://github.com/jm2/tributary/pull/142)).

  Complete; mixed-source UI/commit authority PR #142.

- [x] **P1.5-C** — **Record C — Server-native contract, protocol, and pull authority:** define
  Subsonic native playlist direction, identity, conflict, offline, deletion, unsupported-feature,
  and unlink semantics; implement bounded `getPlaylists`/`getPlaylist` reads; and expose them only
  through an exact-current-session, default-deny registry capability
  ([contract](subsonic-playlist-sync.md); [#143](https://github.com/jm2/tributary/issues/143)).

  Complete; [subsonic-playlist-sync.md](subsonic-playlist-sync.md), PR #144.

- [x] **P1.5-D** — **Record D — Link persistence and atomic pull synchronization:** add dedicated
  non-secret native-playlist link state plus detached Import Copy and read-only Keep Synced manager
  operations. Preserve exact order and duplicates; apply each current pull all-or-none; detect local
  drift before overwrite; retain the last successful snapshot on offline, parse, auth, cancellation,
  or stale-session failure; and represent server deletion without cascading local data.

  Complete; atomic link/pull persistence PR #145.

- [x] **P1.5-E** — **Record E — Server-native playlist UI and lifecycle integration:** add localized
  Import Copy, Keep Synced, Sync Now, conflict/missing/offline status, reconnect refresh, Retry,
  Replace Local with Server, Unlink, and Remove Local Copy flows with accessible end-to-end
  coverage. Do not make linked mirrors editable or expose unsupported adapters/servers as writable
  playlist sources ([#149](https://github.com/jm2/tributary/pull/149), which closes
  [#143](https://github.com/jm2/tributary/issues/143)).

  Complete; coordinated UI/lifecycle delivery PRs #146–#149.

## P2 — User-facing integrations and bounded enhancements

### P2.1 — Migration and listening integrations

- [x] **P2.1-A** — Import Rhythmbox `rhythmdb.xml`, playlists, play counts, and ratings
  transactionally and idempotently, with exact non-guessing matching and actionable
  conflict/unmatched reporting ([#57](https://github.com/jm2/tributary/issues/57),
  [#150](https://github.com/jm2/tributary/pull/150)).

  Complete; [rhythmbox-migration.md](rhythmbox-migration.md), PR #150.

- [ ] **P2.1-B** — Implement Last.fm authorization and protected secret storage,
  now-playing/scrobble thresholds, durable retry/offline behavior, privacy UX, and source-aware
  metadata on authoritative playback events ([contract](lastfm-scrobbling.md);
  [#50](https://github.com/jm2/tributary/issues/50); [foundation
  #151](https://github.com/jm2/tributary/pull/151); [runtime/lifecycle
  slice](https://github.com/jm2/tributary/pull/153); [playback/now-playing
  slice](https://github.com/jm2/tributary/pull/154); [desktop-authorization
  slice](https://github.com/jm2/tributary/pull/155); [playback-ownership
  slice](https://github.com/jm2/tributary/pull/156); [removable-attribution
  slice](https://github.com/jm2/tributary/pull/157); [process-coordinator
  slice](https://github.com/jm2/tributary/pull/158); [headless runtime-bridge
  slice](https://github.com/jm2/tributary/pull/159); [application-owner core
  slice](https://github.com/jm2/tributary/pull/160); [production-composition
  slice](https://github.com/jm2/tributary/pull/165)).

  In flight; issue #50 and [lastfm-scrobbling.md](lastfm-scrobbling.md). The protocol,
  vault, queue, runtime, playback, authorization and application cores are implemented. Durable
  policy generations/migration 20 landed in `e7b07c1`; production is still Dormant. Continue under
  one integration owner through these named slices of this single countable record:

  - **LF1:** exact local/authenticated-remote attribution and the same live policy generation at
    queue capture and dispatch, consuming the existing policy storage.
  - **LF2:** one-shot activation versus successor policy/account generations; typed runtime
    status, disconnect, recovery, and same-account reauthorization controls.
  - **LF3:** localized consent/browser/account UI, authorization-owner construction, atomic
    vault install, different-account purge/install, and missing/corrupt-vault/queue-full UX.
  - **LF4:** package-time credentials/API registration and actual application-to-service
    acceptance: source refusal, durability, revoke/cancel/restart and bridge-before-runtime drain.

  Package credentials remain an external release dependency. Completed internal slices do not
  authorize production enablement before current consent, shared policy and complete acceptance.

### P2.2 — Drag and drop

- [ ] **P2.2-A** — Add accessible multi-selection drag/drop onto local regular playlists, with
  stable occurrence ordering, clear feedback, cancellation, and keyboard-equivalent behavior
  ([#46](https://github.com/jm2/tributary/issues/46)).

  Merged (acceptance pending); PRs #182/#242. Reconcile the existing drag source,
  occurrence ordering, feedback, cancellation, keyboard equivalent, empty-space/header refusal,
  user docs and changelog on the merged head. Keep #46 open for the remaining destinations.

- [ ] **P2.2-B** — Design file-manager export, remote-row drops, and device-copy drops as separate
  authority and transfer policies; implement only the variants whose target semantics are available.

  In flight; #221 / `tr-4cc` is design-only. Accept disclosure and transfer policies
  per destination; export consumes merged #182, device copy depends on P3.2-A. Assign remaining
  application children under #46; unsupported remote writes must remain unavailable.

### P2.3 — Library browsing and presentation

- [ ] **P2.3-A** — Add root-relative folder browsing with multiple-root disambiguation, lazy
  navigation, unavailable/renamed-root behavior, and an explicit omission policy for pathless
  sources ([#14](https://github.com/jm2/tributary/issues/14)).

  Merged (acceptance pending); `455e786` supplies the model and pane. R3–R6 must
  complete actual activation/filtering, Windows paths, authoritative root status and live updates.
  Preserve root containment, lazy/multi-root semantics, keyboard parity and pathless omission.

- [ ] **P2.3-B** — Add album artwork to the browser using a virtualized, accessible UI and bounded
  asynchronous loading/cache with cancellation, placeholders, authenticated-art resolution, and
  persisted layout preferences ([#39](https://github.com/jm2/tributary/issues/39)).

  In flight; #171 / `tr-xaj`, draft redesign. Preserve request-local cancellation,
  recycled-row/cache-hit safety, retained local and authenticated authority, placeholders, bounded
  decoded memory/work, source isolation and filter/persistence correctness. Require a new exact-head
  review plus widget/accessibility evidence; do not duplicate the held implementation.

- [ ] **P2.3-C** — Re-evaluate and implement the independently useful separator, count-opacity, and
  alignment refinements against current GNOME HIG/theme behavior, with visual and accessibility
  review ([#29](https://github.com/jm2/tributary/issues/29)).

  Merged (acceptance pending); #179. Reconcile original behavior and user docs/changelog,
  then record actual visual/high-contrast/accessibility evidence. Display-skipped widget tests do
  not establish final acceptance. Do not dispatch another presentation implementation.

### P2.4 — Audio processing and output protocols

- [ ] **P2.4-A** — Design the equalizer filter graph, band/preset/preamp/clipping contract,
  live-reconfiguration boundary, persistence, and capability matrix for local, AirPlay, Chromecast,
  and MPD outputs ([#49](https://github.com/jm2/tributary/issues/49);
  [contract in progress](equalizer.md)).

  In flight; #183 / `tr-sbp`. Accept the measured DSP/clipping contract before P2.4-B.

- [ ] **P2.4-B** — Implement the supported equalizer path and accessible settings UI, then test
  format changes, gapless navigation, disabled/bypass behavior, clipping policy, and each output's
  supported or explicitly unavailable state.

  In flight; #220 / `tr-dkk`, depends on P2.4-A. Test actual application wiring,
  clipping, format changes, gapless persistence and supported-or-unavailable output states.

- [ ] **P2.4-C** — Open and complete an AirPlay sender design investigation that first resolves the
  current non-shipped `raopsink` seam, then scopes maintained RAOP and/or AirPlay 2 dependencies,
  pairing, encrypted control, audio/timing, licensing, key provenance, packaging, and real-device
  tests.

  In flight; #170 / `tr-vp9`, draft. Design acceptance includes protocol evidence,
  licensing/key provenance, process/native packaging and representative device prerequisites.

- [ ] **P2.4-D** — Implement the selected maintained AirPlay sender path without presenting
  unsupported discovered receivers as playable; keep simultaneous multi-room sync out of scope
  unless separately approved.

  Blocked on accepted P2.4-C; do not implement against a still-rejected sender design.

- [ ] **P2.4-E** — Validate the selected AirPlay interoperability and packaging paths on supported
  platforms and representative receivers, including reconnect, cancellation, authentication failure,
  and actionable diagnostics.

  Blocked on P2.4-D. Record exact artifact, receiver and environment results;
  mocks alone do not complete this existing interoperability record.

- [ ] **P2.4-F** — Add receiver-facing IPv6 Chromecast media tickets where a reachable scoped
  address can be published safely; retain fail-closed omission for unusable endpoints.

  Merged (acceptance pending); #174 supplies target-routed IPv6 tickets. Reconcile
  complete tests/docs/changelog and reachable-address acceptance; retain explicit rejection of
  unusable endpoints, including scoped addresses that cannot be safely published. R8 media
  representation is separate, not missing IPv6 publication.

- [ ] **P2.4-G** — Design and implement an optional detectable MPD exclusive-control/ownership mode
  before enabling automatic orphan cleanup; account for partition-global playback and option
  commands.

  In flight; #173 / `tr-cem`, draft. Preserve explicit consent and safe refusal
  on expired/foreign evidence; supervision is not an atomic partition lock. Require exact-head
  tests for options, cleanup, stop/replacement and supervision loss before relaxing safeguards.

## P3 — Large data-movement epics and engineering follow-ups

### P3.1 — Offline remote media

- [ ] **P3.1-A** — Design persistent source-scoped offline identity, authenticated/resumable
  download jobs, atomic storage, server capability, credential, licensing, and reconciliation
  contracts ([#11](https://github.com/jm2/tributary/issues/11)).

  In flight; #181 / `tr-92a`, draft. Accept resume validators, integrity provenance,
  atomic publication, source identity/licensing, credential policy and quota semantics first.

- [ ] **P3.1-B** — Implement the bounded download/cache engine with restart recovery, integrity
  checks, cancellation, quota/eviction, source replacement, and offline catalogue resolution.

  In flight; #228 / `tr-0ug` (contracts) then #231 / `tr-4q8` (engine), depending on
  P3.1-A. Reconcile overlapping heads and assign durable SQLite/job persistence, authenticated
  backend download and offline playback resolver children. A type surface cannot complete the
  engine contract or establish actual restart, eviction and catalogue behavior.

- [ ] **P3.1-C** — Add accessible download/progress/storage UI and test online-to-offline
  transitions, stale servers, partial files, quota pressure, logout, and cache deletion.

  In flight; #230 / `tr-8h4`, depends on P3.1-B. Its panel still requires window
  wiring. Prove selection → authorized download → durable publish → offline playback, including
  visible retry/cancel/delete, partial files, quota, stale servers and logout. Share the existing
  engine owner; an unattached widget is not a completed product feature.

### P3.2 — Android and device synchronization

- [ ] **P3.2-A** — Build a generic mounted-filesystem transfer planner/executor with retained write
  authority, capacity/conflict policy, atomic copy where possible, progress, cancellation, and
  rollback ([#8](https://github.com/jm2/tributary/issues/8)).

  In flight; #175 / `tr-0na`. Reuse retained mutation authority; prove failure,
  target replacement and detach rollback before enabling device-copy drops or dependent sync.

- [ ] **P3.2-B** — Add MTP discovery and bounded browsing/transfer for typical Android devices
  without treating host paths as portable device identity.

  In flight; #178 / `tr-t3i`, depends on P3.2-A. A transport trait/test adapter is
  insufficient: assign a real transport, native packaging/capabilities, actual object I/O,
  permission/session recovery and attachment wiring. Require representative Android evidence.

- [ ] **P3.2-C** — Add playlist mapping, incremental state, conflict resolution, and explicitly
  opted-in auto-sync with safe attach/detach recovery.

  In flight; #180 / `tr-bau`, depends on P3.2-A/B. Integrate planner/executor and
  controls; prove persistent incremental state, conflicts, attach/detach recovery and explicit
  auto-sync consent through the application and a real device.

### P3.3 — Authority and queue extensions

- [ ] **P3.3-A** — Add typed retained mutation authority before enabling Properties/tag writes for
  pathless removable rows; revalidate the mount, ancestry, exact file, write rights, and replacement
  target through commit.

  In flight; #232 / `tr-mms4s`. Coordinate common authority with R1 without treating
  path-only local preflight as removable authority. Retain stale-target rollback and repeat-save tests.

- [ ] **P3.3-B** — If product-approved, turn multi-file OS-open deliveries into an
  occurrence-preserving ephemeral queue; keep the current first-valid-file behavior documented until
  then.

  Blocked on product decision. Preserve first-valid-file/discard documentation until
  an occurrence-preserving queue contract is accepted; do not quietly expand OS-open behavior.

### P3.4 — Maintenance and coverage

- [x] **P3.4-A** — Re-evaluate `paste`, the fuzz-only `proc-macro-error2` path, and the inactive
  lockfile-only `rkyv` advisory by 2026-09-01 or the next release. Revisit immediately before
  enabling `rkyv` serialization or accepting Decimal archive input
  ([#218](https://github.com/jm2/tributary/pull/218)).

  Complete. The September review removed fuzz-only `proc-macro-error2`, retained the
  justified compile-time `paste` edge and inactive lock-only `rkyv` exception. See historical evidence.

- [ ] **P3.4-B** — Re-evaluate the retained compile-time `paste` edge and inactive lock-only `rkyv`
  exception by 2026-12-01 or before the next release, whichever comes first. Revisit immediately
  before enabling `rkyv` serialization or accepting Decimal archive input.

  Ready at release or 2026-12-01, whichever is first; inspect both workspaces.

- [ ] **P3.4-C** — Remove the macOS GStreamer channel-cap workaround only after the upstream fix is
  in the supported runtime floor and passes affected multi-channel hardware testing.

  Blocked on supported upstream/runtime and affected multichannel hardware evidence.
  Prove playing/paused switch/unplug/replug; default-output changes must not silently remove the cap.

- [x] **P3.4-D** — Add a direct end-to-end watcher-backlog/root-confirmation ordering harness if its
  incremental coverage remains worth the platform-fixture cost. The
  `marker_mutation_confirms_root_before_backlog_incrementals_end_to_end` harness drives the real
  watcher loop through a deterministic synthetic event channel, so the ordering contract is covered
  without a live platform watcher fixture.

  Complete; the existing real-loop harness and merged #222 Name::Any/trust-order tests
  provide evidence. Keep physical watcher/device behavior under release validation, not another harness.

## Q — Engineering acceptance and resource coverage

These seven records are newly scheduled. Each needs a scoped issue/bead and owner before activation;
the review proposal supplies rationale. Operator and physical validation are counted separately.

- [ ] **Q1 — Display-backed GTK interaction gate** (P2; Ready; issue/bead unassigned).
  Run production widgets under a real CI display with one-thread GTK ownership and fail-on-skip
  reporting. Cover R3/R4, selection restoration, drag/drop, settings and close. Build on #179's
  consolidated tests. Record native keyboard/screen-reader, contrast, scaling and long-translation
  smoke checks separately; green skipped bodies do not establish interaction acceptance.

- [ ] **Q2 — Explicit security audits for both Cargo lockfiles** (P2; Ready; issue/bead unassigned).
  Audit root and independent fuzz graphs with scoped advisory exceptions. Prove the fuzz lock is
  actually selected and a graph-specific finding cannot hide behind root-lock coherence. This is
  a missing audit boundary, not evidence of a currently exploitable dependency.

- [ ] **Q3 — Broader bounded production-parser fuzzing** (P3; Ready; issue/bead unassigned).
  Extend existing DMAP coverage through separate XML, strict Last.fm response, and URL/ticket/range
  slices. Use production code, corpus seeds, memory/time limits and retained crash artifacts, with
  no live servers/credentials and no weakening of parser authority/privacy boundaries.

- [ ] **Q4 — Measured large-library responsiveness** (P2; Ready; issue/bead unassigned).
  Establish fixed 10k/100k-track and delayed filesystem/backend fixtures. Measure time to interactive,
  source/filter/rebuild latency, main-loop stalls, retained rows/bytes, update bursts, cancellation
  and command admission. Agree runner-specific budgets before optimizing failing paths. Coordinate
  R9; module line counts alone do not justify a general rewrite.

- [ ] **Q5 — Process/ticket media-relay resource limits** (P3; Ready, design first;
  issue/bead unassigned).
  Bound requests, authority jobs and body workers before spawning/upstream admission. Hold permits
  through body completion; define slow-consumer refusal/cancellation and kernel-I/O limits. Test
  valid parallel ranges, stalled readers, disconnect capacity recovery and ordinary concurrent seeks.
  Per-response buffers are not a process-wide bound.

- [ ] **Q6 — Aggregate remote catalogue limits and explicit partialness** (P3; Ready, design first;
  issue/bead unassigned).
  Define total rows/bytes/work and repeated-page detection for Jellyfin/Plex/Subsonic. Reject or
  visibly mark limits/partial results; partialness must not become authoritative absence. Test
  repeated/oversized pages, sections, large metadata, cancellation and memory. Coordinate registry
  and offline consumers before changing publication semantics.

- [ ] **Q7 — Backlog/issue/bead consistency checks** (P2; Ready; issue/bead unassigned).
  Validate unique IDs, literal counters, internal links and issue/bead/PR mappings for active work.
  Surface merged-but-unreconciled acceptance and stale review heads without auto-closing parents
  or assigning workers. Keep authoritative transitions in GitHub and the Gas City ledger; optional
  synchronization must not create another dispatcher.

## Operator and release evidence

These are rollout/validation obligations, not additional feature boxes or proof of shipped behavior.
Record owner, date, exact commit/artifact, CI/device/environment, result and outstanding failures
before marking any row validated. Live Gas City configuration was unavailable at audit; this file
documents requirements, not confirmation of deployed settings.

| ID | State / owner boundary | Required evidence |
| --- | --- | --- |
| O1 | Pending; rollout owner, #225/#237 | Agreed gate policy and live refusal/admission proof |
| O2 | Pending; city config owner | Live CI timeout, check names and repair/hold policy |
| V1 | Pending; removable test owner | Real browse/play/containment and detach/reconnect |
| V2 | Pending; Flatpak test owner | Installed portal/custom-root/USB permission behavior |
| V3 | Pending; Windows package owner | Packaged real DAAP/Subsonic playback and reconnect |
| V4 | Pending; macOS package owner | Real protected playback and output-device transitions |

- **O1:** reconcile out-of-band GLM versus proposed bot gates, read back agreed check/app bindings
  and native auto-merge settings, and prove refusal for missing/failing required evidence plus
  admission for complete accepted evidence. Reuse #225/#237 and the rollout owner.
- **O2:** verify the documented 3600-second timeout and exact check names in live city.toml;
  pending remains non-green. Preserve finite repair paths and current worker/review holds.
- **V1–V3:** retain the exact three archived environment-validation contracts; close each archive
  record only after evidence is recorded here and there. Automated bundle/containment probes are
  prerequisites, not substitutes for these installed/hardware/server checks.
- **V4:** use a bundle without Homebrew after #243; test protected remote playback and output
  switch/unplug/replug while playing/paused, including affected multichannel hardware.

Current [refinery policy](refinery-config.md) uses out-of-band GLM review and retires repo-owned AI
review workflows. #225/#237 propose a different policy; O1 must resolve it with the rollout owner.
Main's ruleset currently requires seven CI contexts including MSRV, not the full hosted matrix.
Do not silently restore retired reviewers or treat optional external statuses as accepted review.

## Explicitly outside this backlog

- Apple signing/notarization remains a distribution decision outside implementation counts;
  record support/known limits with release evidence. Release-tag dispatch was proven in #224 /
  run 33781875201; do not reopen its completed dry-run work.
- Backup/restore needs a product/design decision on consistent snapshots, versions, integrity,
  retention, recovery UI and private Last.fm queue treatment before implementation is scheduled.
- Direct Apple/iTunes XML, Google Takeout CSV, M3U, service-URL input and fuzzy matching remain
  unscheduled. XSPF is the supported interchange path.
- Automount/eject, markerless read-only root enrollment, stronger native removable IDs and saved
  endpoint rebind remain candidates until scoped. P3.3-B retains its explicit product gate.
- A capability-derived native audio-plugin allowlist needs a proven container/source/output matrix
  before replacing the existing fail-closed component deny policy.

## Implementation log

The [preserved implementation log](task-implementation-history-2026-09-09.md#implementation-log)
contains earlier PR delivery evidence and historical release counts.

- **2026-09-09:** adopted the review expansion, filed bugs #248–#258, assigned stable IDs, and
  exposed Gas City dependencies/holds and release evidence. Preserved all 39 original record states.
  Corrected the old count to 16/39; eleven corrective and seven engineering additions yield
  16/57 (28.1%). This documentation update closes no feature acceptance.
