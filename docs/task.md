# Tributary remediation tracker

Source review: [`CODE_REVIEW_2026-07-10.md`](../CODE_REVIEW_2026-07-10.md)

Reviewed commit: `598b332d31c6206aea620aa951b78335e4d659ed`
Created: 2026-07-10

## How to use this file

- Keep tasks unchecked until their acceptance criteria and listed verification are complete.
- Add the implementing commit or PR beside a completed task.
- If scope changes, update the review document or record the decision under **Decisions**.
- Run the global validation gate after every milestone.
- Do not combine the architecture milestone with unrelated bug fixes.

Status summary:

- [ ] P0 release blockers complete
- [ ] P1 correctness and security complete
- [ ] P2 resilience and packaging complete
- [ ] P3 architecture and integration coverage complete

## P0 — Release blockers

### P0.1 Fix playlist-position migration

- [x] Replace the self-referential rank update with a materialized snapshot.
- [x] Wrap rank normalization and unique-index creation in an explicit transaction.
- [x] Preserve existing playlist order when row insertion order differs.
- [x] Add migration fixtures for gaps, duplicates, reordered rows, multiple playlists, and
  an empty table.
- [x] Verify a failed migration cannot leave partially updated positions.
- [x] Record implementation: PR #68; 10 focused migration tests.

Acceptance criteria: upgrading a v0.5.0-style database always yields a unique contiguous
`0..N` position sequence per playlist without changing the intended order.

### P0.2 Make initial reconciliation non-destructive

- [x] Collect traversal errors and completion state per configured root.
- [x] Skip stale deletion for any root whose traversal was incomplete.
- [x] Correctly reconcile a healthy root containing zero audio files.
- [x] Define and persist availability/mount identity for removable or network roots.
- [x] Replace reboot/remount-sensitive legacy device identities with a versioned, root-owned
  marker; create markers only on explicitly configured roots, convert only a still-matching
  legacy root, and make the fresh marker-backed conversion scan non-destructive.
- [x] Load persisted root authorization once per watcher batch, prefer the most-specific root,
  and retain fail-closed invalidation for the rest of the batch.
- [x] Add tests for missing, empty, permission-denied, partially unreadable, and overlapping
  roots, plus marker corruption/duplication/conversion and watcher-cache invalidation.
- [ ] Add an explicit trust/re-enrollment flow for roots inherited from a pre-identity database;
  content similarity is not accepted as proof of physical volume identity.
- [ ] Pin reconciliation and watcher mutations to a root handle or equivalent mount generation
  on every supported platform; Linux has mount-instance guards, while portable Unix ABA
  resistance remains incomplete.
- [x] Record implementation: safety core and review follow-ups are in PR #68 with 44 focused
  tests; explicit legacy trust and portable root pinning remain open.

Acceptance criteria: Tributary never deletes persisted track metadata based on an incomplete
view of a library root, while intentional offline deletion is eventually reflected.

### P0.3 Preserve DAAP session lifetime

- [x] Retain the connected backend/session for as long as the source is connected.
- [x] Remove logout network activity from `DaapBackend::drop`.
- [x] Populate and/or replace the explicit disconnect path with owned backend shutdown.
- [x] Resolve stream/artwork URLs from the live session at playback time.
- [x] Add a mock DAAP lifecycle test covering connect, sync, play, and disconnect.
- [x] Record implementation: PR #68; 7 focused lifecycle and
  replacement-race tests.

Acceptance criteria: a DAAP session remains valid after library synchronization and is
logged out exactly once on explicit disconnect or controlled shutdown.

### P0.4 Introduce stable playback-session identity

- [x] Replace the raw visible-row `current_pos` identity with stable source and track IDs.
- [x] Store a playback queue snapshot independent of sorting/filtering/navigation.
- [x] Define Next, Previous, shuffle, repeat-one, and repeat-all semantics after view changes.
- [x] Make reselecting the current output a no-op.
- [x] Explicitly transfer or clear playback when the output target changes.
- [x] Add tests for sort, filter, source change, output change, EOS, and external-file playback.
- [x] Record implementation: PR #68; 25 focused UI/output tests.

Acceptance criteria: view mutations never change the identity of the playing track or the
meaning of queue navigation.

### P0.5 Fix recycled sidebar action handlers

- [x] Connect the action button once or disconnect handlers on every unbind.
- [x] Ensure each click resolves only the currently bound `SourceObject`.
- [x] Cover delete, DAAP eject, playlist creation, and forced remove/reinsert rebinds.
- [x] Add a GTK test or focused harness that repeatedly recycles the same list item.
- [x] Record implementation: PR #68; focused recycling harness.

Acceptance criteria: one click emits exactly one action for the currently displayed row.

### P0.6 Lock down release build inputs and credentials

- [x] Vendor or immutably pin the Flatpak Cargo generator and verify its checksum.
- [x] Pin Python build dependencies or run them from a reviewed lock/environment.
- [x] Set `persist-credentials: false` on build-job checkouts.
- [x] Move `contents: write` to a minimal publication job.
- [x] Give all build jobs `contents: read`.
- [x] Record implementation: PR #68; YAML, checksum, and contract
  checks passed locally.

Acceptance criteria: release build jobs execute only repository or immutable verified code
and cannot access a repository write credential.

### P0.7 Honor the manual release tag

- [x] Compute a single validated build ref for release and manual-dispatch events.
- [x] Pass the ref to every checkout in the release workflow.
- [x] Derive every package version from the checked-out source/tag.
- [x] Reject a missing or malformed requested tag.
- [ ] Add a dry-run/manual workflow test demonstrating that tag X builds tag X.
- [ ] Record implementation: workflow contract implemented in PR #68; live
  manual-dispatch verification pending after push.

Acceptance criteria: all artifacts in a run are built from the same requested immutable ref
and carry the same version.

### P0.8 Clear the dependency audit failure

- [x] Update `crossbeam-epoch` to `>= 0.9.20` through its dependency chain.
- [x] Update the locked `quinn-proto` to `>= 0.11.15` or remove the unused optional graph.
- [x] Review the `anyhow`, `paste`, and `proc-macro-error2` warnings and document upstream
  disposition.
- [x] Run `cargo audit` successfully.
- [x] Record implementation: PR #68; `cargo audit` passes with two
  documented informational warnings.

Audit disposition recorded 2026-07-10:

| Advisory | Dependency path | Disposition | Revisit by |
|---|---|---|---|
| [`RUSTSEC-2026-0190`](https://rustsec.org/advisories/RUSTSEC-2026-0190) (`anyhow`) | direct and transitive | Fixed by locking and requiring `anyhow >= 1.0.103`. | Closed |
| [`RUSTSEC-2024-0436`](https://rustsec.org/advisories/RUSTSEC-2024-0436) (`paste`) | `lofty 0.24.0 -> paste 1.0.15` | Informational/unmaintained, with no patched `paste` release. Track Lofty migration to a maintained replacement; no direct Tributary use. | 2026-10-10 or next release, whichever comes first |
| [`RUSTSEC-2026-0173`](https://rustsec.org/advisories/RUSTSEC-2026-0173) (`proc-macro-error2`) | `sea-orm 1.1.20 -> sea-bae 0.2.1` | Informational/unmaintained compile-time macro dependency. Track SeaORM's removal or evaluate the SeaORM 2 migration. | 2026-10-10 or next release, whichever comes first |

Acceptance criteria: the CI security-audit job passes with every remaining ignored advisory
explicitly justified and time-bounded.

## P1 — Correctness and security

### P1.1 Add the real playlist-entry track foreign key

- [x] Rebuild `playlist_entries` with `track_id -> tracks(id) ON DELETE SET NULL`.
- [x] Null existing dangling IDs before enabling the constraint.
- [x] Reconcile newly orphaned entries after scans and watcher insertions.
- [x] Test delete, rename, re-add, and full rebuild behavior.
- [x] Record implementation: commit `8ec84a5`; 12 focused migration,
  reconciliation, and watcher-batch tests.

### P1.2 Preserve identity for authoritative filesystem renames

- [x] Preserve event order and tracker metadata; normalize authoritative Linux and Windows
  rename pairs without processing duplicate halves.
- [x] Transactionally update same-root, same-format paired file paths while preserving UUID,
  `date_added`, play count, playlist linkage, and mutable metadata.
- [x] Reconcile directory changes, unpaired/unknown renames, cross-root moves, and format changes
  with one hardened authoritative scan per watcher batch rather than guessing identity.
- [x] Disable watcher symlink following so incremental indexing matches authoritative scans.
- [x] Cover cross-platform event shapes, destination replacement, guard rejection, SQL rollback,
  and playlist-FK preservation with eight focused tests.
- [x] Preserve descendant IDs for paired directory renames by retargeting every safely mapped
  indexed descendant in one transaction after a complete scoped traversal.
- [x] Refresh already-captured local/playlist queue items by stable track ID after an
  ID-preserving committed rename, so Next/EOS uses the new URI.
- [x] Record implementation: stacked P1.2 commits; 24 additional focused directory-rename,
  batch-deferral, no-follow, scoped-scan, playlist-projection, and queue-refresh tests, for 32
  focused P1.2 tests total.

### P1.3 Close the scan/watcher handoff gap

- [x] Install the watcher before initial enumeration.
- [x] Buffer and replay events generated during the scan.
- [x] Reconcile after watcher errors or overflow.
- [x] Use bounded/coalescing event delivery where appropriate.
- [x] Add race-oriented tests.
- [x] Record implementation: commit `4eb79d0` plus review follow-ups; seven focused ingress,
  replay, registration-retry, stream-loss, marker-mutation, and race tests.

### P1.4 Enforce exact-origin authenticated redirects

- [x] Disable automatic `Referer` for every app-owned credential-bearing HTTP client.
- [x] Compare scheme, hostname, and effective port.
- [x] Forbid HTTPS-to-HTTP downgrade.
- [x] Apply the policy to app-owned API, authentication, artwork, and DAAP clients.
- [ ] Route credential-bearing playback streams through the P1.6 app-owned proxy so local,
  AirPlay, MPD, and Chromecast no longer depend on redirect policies Tributary cannot control.
- [x] Strip credential-bearing URLs from every retained/formatted HTTP or pipeline error.
- [x] Stop logging raw DAAP session IDs and authenticated MPD commands.
- [x] Add redirect matrix tests using mock servers.
- [x] Record implementation: commit `eb5458f`; 17 focused origin, redirect, Referer, header,
  redaction, userinfo, DAAP, and MPD tests.

### P1.5 Enforce response limits while streaming

- [ ] Replace `Content-Length`-only checks with counted streaming reads.
- [ ] Apply caps to API JSON, DAAP, authentication, radio, and album-art responses.
- [ ] Add overall deadlines in addition to idle timeouts.
- [ ] Test missing, false, and oversized `Content-Length`, plus endless chunked bodies.
- [ ] Record implementation: _pending_

### P1.6 Stop exposing broad bearer tokens to receivers

- [ ] Define an opaque short-lived media proxy/ticket design.
- [ ] Keep backend credentials outside generic `Track` values.
- [ ] Proxy credential-bearing streams for local, AirPlay, MPD, and Chromecast, and apply the
  shared exact-origin policy to the proxy's upstream client.
- [ ] Expire/revoke proxy registrations when playback/session ends.
- [ ] Threat-model spoofed and compromised LAN receivers.
- [ ] Record implementation: _pending_

### P1.7 Serialize Chromecast lifecycle and commands

- [ ] Use one ordered worker/session for load, play, pause, seek, volume, and stop.
- [ ] Check cancellation before each external side effect and emitted event.
- [ ] Prevent stale loads from launching or replacing newer media.
- [ ] Ensure every failure terminates in a coherent state, never Error then Buffering.
- [ ] Add delayed-device and supersession tests.
- [ ] Record implementation: _pending_

### P1.8 Implement authoritative MPD state

- [ ] Serialize MPD commands through one worker or persistent connection.
- [ ] Emit Playing, Paused, Stopped, position, duration, and completion events.
- [ ] Clear buffering timers on success and error.
- [ ] Redact authenticated URLs from command logs.
- [ ] Add fake-server tests including slow and reordered commands.
- [ ] Record implementation: _pending_

### P1.9 Prevent stale async source rendering

- [ ] Attach a source key/generation to playlist, radio, and remote loads.
- [x] Refresh already-open playlist URIs after an ID-preserving local rename and overlay committed
  URIs onto an in-flight result before publication.
- [ ] Reload an active playlist after watcher reconciliation remints or relinks track IDs.
- [ ] Reject an in-flight playlist result when its source generation is stale, including when it
  would render pre-rename rows after the refreshed model or replace a newer source selection.
- [ ] Cache completed results even when no longer active.
- [ ] Render only if the requested source remains selected.
- [ ] Reuse the active-key guard pattern already present in USB loading.
- [ ] Add navigation-race tests.
- [ ] Record implementation: _pending_

## P2 — Resilience, data semantics, and packaging

### P2.1 Correct smart-playlist semantics

- [ ] Parse dates/timestamps instead of comparing strings.
- [ ] Define date-only versus instant behavior and timezone rules.
- [ ] Validate relative-date amounts and use checked arithmetic.
- [ ] Select/truncate by limit criteria before applying final compound sort.
- [ ] Implement snapshot behavior for `live_updating = false` or remove the option.
- [ ] Add combined rule/limit/sort/date tests.
- [ ] Record implementation: _pending_

### P2.2 Make playlist import/export transactional and deterministic

- [ ] Define supported source formats and provide adapters or actionable conversion guidance
  for Apple Music XML and YouTube Music exports.
- [ ] Export through a sibling temporary file and atomic replacement.
- [ ] Prefer an exact existing file path before metadata matching.
- [ ] Enforce the documented duration tolerance and deterministic tie-breaking.
- [ ] Return database errors rather than treating them as no-match.
- [ ] Import playlist and entries in one transaction.
- [ ] Preserve unmatched entries for later reconciliation.
- [ ] Surface matched, unmatched, and failed counts.
- [ ] Record implementation: _pending_

### P2.3 Harden tag writes

- [ ] Validate all numeric edits before copying or modifying a file.
- [ ] Apply or remove the declared album-artist edit.
- [ ] Use an exclusively created random sibling temp path.
- [ ] Guarantee cleanup on every failure path.
- [ ] Preserve permissions and define durability/fsync behavior accurately.
- [ ] Add concurrent-write and injected-failure tests.
- [ ] Record implementation: _pending_

### P2.4 Make removable-media browsing safe and asynchronous

- [ ] Disable symlink following during device scans.
- [ ] Verify canonical descendants remain on the selected mount/device.
- [ ] Move mount discovery and traversal off the GTK thread.
- [ ] Use platform mount/volume APIs rather than directory heuristics.
- [ ] Add malicious symlink, stale mount, and duplicate-device tests.
- [ ] Record implementation: _pending_

### P2.5 Repair Flatpak behavior and local build path

- [ ] Put the pinned generator where local and CI builds both use it.
- [ ] Generate `build-aux/flatpak/cargo-sources.json` consistently.
- [ ] Define narrow USB/removable-media permissions or a portal workflow.
- [ ] Define writable custom-library behavior for tag editing.
- [ ] Run a local Flatpak build and smoke-test USB/custom-library behavior.
- [ ] Record implementation: _pending_

### P2.6 Synchronize packaging metadata

- [ ] Fix RPM `Version`, `Source0`, and `%autosetup` naming.
- [ ] Raise GTK runtime minimum to 4.16.
- [ ] Raise libadwaita runtime minimum to 1.6.
- [ ] Add `%U` or `%F` to the desktop `Exec` line.
- [ ] Add the required `AudioVideo` desktop category.
- [ ] Add the 0.5.0 AppStream release entry.
- [ ] Update README Rust requirement from 1.80 to 1.85.
- [ ] Add CI on the declared Rust 1.85 MSRV.
- [ ] Record implementation: _pending_

### P2.7 Fix platform cache paths

- [ ] Store GStreamer registries in a per-user cache directory.
- [ ] Avoid writes inside `/Applications` and Program Files.
- [ ] Generate or patch the macOS pixbuf loader cache for the installed absolute bundle path.
- [ ] Verify macOS signature integrity after first launch.
- [ ] Record implementation: _pending_

## P3 — Architecture and integration coverage

### P3.1 Introduce a source/session registry

- [ ] Define stable source IDs and backend-native track IDs.
- [ ] Store `Arc<dyn MediaBackend>` or a deliberate session abstraction per source.
- [ ] Remove long-lived authenticated URLs from the generic `Track` model.
- [ ] Resolve playable URLs/tickets at playback time.
- [ ] Resolve local/playlist media by stable track ID at playback, navigation, and receiver-load
  time so fallback reconciliation and in-flight casts cannot retain dead file paths.
- [ ] Centralize source refresh, cancellation, disconnect, and failure state.
- [ ] Decide how local, radio, and external-file sources fit the same lifecycle.
- [ ] Record architecture decision: _pending_
- [ ] Record implementation: _pending_

### P3.2 Make the backend abstraction real and stable

- [ ] Construct and use `LocalBackend` through the same integration boundary.
- [ ] Replace ephemeral album/artist UUIDs with stable identities.
- [ ] Group local albums by a disambiguating key, not title alone.
- [ ] Implement or remove unsupported trait methods.
- [ ] Align README architecture claims with actual code.
- [ ] Record implementation: _pending_

### P3.3 Add network integration harnesses

- [ ] Mock Subsonic, Jellyfin, Plex, DAAP, Radio-Browser, and geolocation services.
- [ ] Cover auth, redirect, timeout, body cap, pagination, partial failure, and reverse-proxy
  prefix behavior.
- [ ] Cover DAAP malformed nested containers and session expiration.
- [ ] Record implementation: _pending_

### P3.4 Add UI/output integration harnesses

- [ ] Cover GTK list-item recycling and stale callback prevention.
- [ ] Cover playback-session behavior across sorting/filtering/navigation.
- [ ] Cover output transfer and reselect semantics.
- [ ] Cover fake MPD and delayed Chromecast state machines.
- [ ] Cover stale album-art and source-result generations.
- [ ] Add keyboard context-menu and slider accessibility checks.
- [ ] Record implementation: _pending_

### P3.5 Make coverage reporting representative

- [ ] Stop excluding all UI, remote backends, migrations, desktop integration, and `main.rs`
  from the only coverage report.
- [ ] Split pure unit and integration coverage if platform constraints require it.
- [ ] Establish a documented baseline and ratchet policy.
- [ ] Record implementation: _pending_

## Global validation gate

Run before marking any milestone complete:

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `cargo clippy --release -- -D warnings`
- [x] `cargo test --all-targets`
- [x] `cargo test --release`
- [x] `cargo audit`
- [ ] `desktop-file-validate data/io.github.tributary.Tributary.desktop`
- [x] `appstreamcli validate --no-net data/io.github.tributary.Tributary.metainfo.xml`
- [x] Targeted migration upgrade tests
- [x] Targeted mock-network tests
- [x] Targeted GTK/output lifecycle tests
- [ ] Packaging dry runs for affected targets
- [x] Confirm `git diff --check` is clean

`desktop-file-validate` still reports the pre-existing missing `AudioVideo` category tracked
by P2.6. Packaging dry runs and the live manual release-workflow run remain outstanding.

## Decisions

Record scope or design decisions here so deferred work is explicit.

- 2026-07-10 — Implemented P0.1, P0.3-P0.6, and P0.8 in PR #68. P0.7's
  workflow contract is implemented, but its live manual-dispatch acceptance test requires a
  pushed ref and remains open.
- 2026-07-10 — P0.2 now fails closed for incomplete traversal, unavailable/replaced roots,
  nested mounts, mount-table failures, and ambiguous legacy databases. Legacy roots with
  existing metadata are intentionally not made deletion-authoritative from content heuristics;
  explicit trust UX and portable root-handle pinning keep P0.2 open.
- 2026-07-10 — Linux mount IDs are treated as ephemeral traversal generations, never durable
  volume identity. Stable root identity is now a versioned marker stored on the library root,
  so a normal reboot or remount does not silently replace the intended root. Duplicate,
  malformed, oversized, symlink/reparse-point, and non-regular markers fail closed through a
  bounded single-handle read; legacy conversion never indexes or deletes rows on its first
  marker-backed scan.
- 2026-07-11 — Watcher events share one deepest-root-first persisted authorization snapshot per
  debounced batch. Root-marker control events and failed identity probes invalidate that
  snapshot before database I/O, so later events in the batch remain fail closed.
- 2026-07-10 — Output changes clear the active playback session; they do not attempt a
  best-effort transfer between unlike output implementations.
- 2026-07-10 — Generation filtering prevents stale receiver events from mutating Tributary.
  Ordered Chromecast command side effects and authoritative MPD state remain P1.7 and P1.8.
- 2026-07-10 — The remaining `cargo audit` warnings are unmaintained transitive dependencies,
  not known vulnerabilities; their owners and 2026-10-10 review deadline are recorded under
  P0.8.
- 2026-07-12 — Track deletion now preserves playlist-entry identity, order, and fingerprint by
  nulling the real track foreign key. Scan and watcher-batch reconciliation relink only when
  fingerprint plus optional duration identifies exactly one current track; ambiguous matches
  remain orphaned. Stable track identity across filesystem renames remains P1.2; safely
  refreshing an already-open playlist after background reconciliation remains P1.9.
- 2026-07-12 — P1.2 preserves identity only for authoritative same-root, same-format pairs:
  tracked Linux events and strictly adjacent Windows rename halves. Unpairable macOS/BSD events,
  cross-root moves, and format changes use a full hardened scan and never infer identity from
  tags.
- 2026-07-12 — A paired directory rename now moves every safely mapped indexed descendant in one
  transaction. Each descendant is moved only when a completed scoped traversal of the destination
  observed a real file at its mirrored path. This is a path-based observation, not an inferred
  metadata match; live filesystem handles retained by the traversal are revalidated before the
  database transaction commits. A descendant with no such file is left in place for reconciliation
  rather than followed to a path that may not exist, and destination files no row claims are
  upserted normally, so an album gaining a file while the app was closed does not defeat identity
  preservation. Paths are matched component-wise in the database's existing lossy namespace, so
  already-persisted non-UTF-8 paths remain matchable subject to that namespace's collision limits,
  and `/music/Album` cannot capture `/music/Album2`. Pairs nested inside a renamed directory, and
  subtrees owning another persisted root or mount, remain fail-closed: the watcher cannot order
  them, so they reconcile.
- 2026-07-12 — Directory-rename halves are deferred, not reconciled on sight. A vanished
  directory and a deleted cover image are indistinguishable when the path is already gone, so
  the batch decides only once every event in the debounce window has arrived; anything no
  authoritative pair claimed still forces the guarded rescan.
- 2026-07-12 — Directory scans retain cross-platform filesystem-object handles for the
  destination and every mapped audio file, then reopen and compare them in the transaction's
  final guard. A removal, replacement, symlink/reparse point, or directory swap therefore rolls
  the identity update back. Files with their own event in the same watcher batch are excluded from
  identity mapping and take the normal parse/reconciliation path. The same object comparison also
  authorizes case-only directory renames on case-insensitive filesystems without accepting a
  copied or recreated source.
- 2026-07-12 — Watcher upserts classify paths with no-follow metadata before parsing and again
  before persistence. Missing audio paths retain the guarded-delete behavior; symlinks, Windows
  reparse points, unexpected path types, and metadata errors force authoritative reconciliation.
- 2026-07-12 — A committed bulk rename publishes one library snapshot rather than a per-row
  event storm, and an already-captured playback queue re-resolves its items from it by stable
  track ID, in place. Already-open playlist rows are retargeted by the same stable ID, and an
  in-flight playlist result overlays committed local URIs before it can render. Same-key request
  generations and post-reconciliation reloads remain P1.9. A rename that falls back to
  reconciliation still mints new track IDs, so the ID-based refresh cannot repair a queue captured
  before it; recovery requires rebuilding it from a refreshed source model. Stable-ID resolution
  at playback, navigation, and receiver-load time remains P3.1 rather than changing queue semantics
  here.
- 2026-07-12 — P1.3 installs each watcher before enumeration, retries roots that become available
  during enumeration, and replays its bounded, ordered ingress after the initial snapshot. Notify
  errors, rescan flags, and queue overflow discard the incomplete incremental batch and retry a
  hardened scan before any later incremental mutation; marker mutations take the same fail-closed
  route. A rename can still lose its old UUID if the
  initial destructive scan has already deleted the source row before the buffered pair is replayed.
  The resulting filesystem/database state is reconciled, but eliminating that narrow identity
  boundary requires a future two-phase, non-destructive bootstrap scan rather than guessing from
  file metadata.
- 2026-07-12 — P1.4 centralizes every app-owned credential-bearing reqwest client behind a
  no-`Referer`, ten-hop, exact-origin policy comparing HTTP(S) scheme, normalized host, and
  effective port. Request URLs are removed before reqwest errors are formatted or retained;
  URL userinfo and backend query credentials are rejected or redacted; DAAP session IDs and MPD
  command arguments are no longer logged; and GStreamer errors use stable URL-free diagnostics.
  Direct playback remains deliberately open under P1.6: local and AirPlay streams are fetched by
  GStreamer, while MPD and Chromecast fetch on external receivers, so Tributary cannot enforce an
  upstream redirect callback on those transports until it owns the stream through a short-lived
  proxy/ticket. Disabling redirects only for GStreamer would be both incomplete and potentially
  breaking, so the playback-stream checkbox remains explicit rather than being claimed complete.

## Completed work log

Add one line per completed task:

| Date | Task | Commit/PR | Notes |
|---|---|---|---|
| 2026-07-10 | P0.1 | PR #68 | Transactional, deterministic, retry-safe migration with focused upgrade fixtures. |
| 2026-07-10 | P0.3 | PR #68 | Owned DAAP registry, generation-safe sync, live URL resolution, and exactly-once shutdown. |
| 2026-07-10 | P0.4 | PR #68 | Stable queue/session identity, generation-filtered events, and deterministic output reset. |
| 2026-07-10 | P0.5 | PR #68 | One setup-time sidebar handler with current-item resolution and recycling tests. |
| 2026-07-10 | P0.6 | PR #68 | Immutable release inputs and publication-only repository credentials. |
| 2026-07-10 | P0.8 | PR #68 | Patched dependency graph and time-bounded informational advisory dispositions. |
| 2026-07-12 | P1.1 | `8ec84a5` | Transactional, retry-safe track-FK rebuild with dangling-link cleanup, index preservation, and scan/watcher reconciliation. |
| 2026-07-12 | P1.2 | `93d03bf`, `b961b7c`, `17babaf`, `000d9c0` | Identity preserved across authoritative paired file and directory renames; queue and active-playlist snapshots re-resolve ID-preserving committed changes by stable track ID. |
| 2026-07-12 | P1.3 | `4eb79d0` | Watchers install before scanning; bounded nonblocking ingress replays ordinary events and routes overflow, backend loss, rescan notices, and marker changes through retrying authoritative reconciliation. |
| 2026-07-12 | P1.4a | `eb5458f` | Exact-origin/no-Referer policy and URL-free errors/logging cover every app-owned credential HTTP fetch; receiver-controlled playback streams remain tied to P1.6. |
