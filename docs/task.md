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
- Add a `CHANGELOG.md` entry in the same commit as any user-visible fix. The remediation
  work through P1.7 landed without one, and the changelog silently drifted four months
  behind the code; a user could not tell that the migration corruption or the destructive
  reconciliation had ever been fixed.

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
- [x] Record implementation: PR #68; `cargo audit` passes with its remaining warnings
  documented below.
- [x] Re-close the disposition table, which had drifted after 2026-07-10 (found 2026-07-13):
  `cargo audit` reports **three** allowed warnings, not two — `spin` was yanked — and
  `.cargo/audit.toml` suppressed `RUSTSEC-2023-0071` with no justification or revisit date in this
  table, which the acceptance criteria requires. Both are now recorded below, and the `rsa`
  suppression carries its real reason (lockfile-only, never compiled) rather than the stale
  "we don't use MySQL" comment.

Audit disposition recorded 2026-07-10, amended 2026-07-13:

| Advisory | Dependency path | Disposition | Revisit by |
|---|---|---|---|
| [`RUSTSEC-2026-0190`](https://rustsec.org/advisories/RUSTSEC-2026-0190) (`anyhow`) | direct and transitive | Fixed by locking and requiring `anyhow >= 1.0.103`. | Closed |
| [`RUSTSEC-2024-0436`](https://rustsec.org/advisories/RUSTSEC-2024-0436) (`paste`) | `lofty 0.24.0 -> paste 1.0.15` | Informational/unmaintained, with no patched `paste` release. Track Lofty migration to a maintained replacement; no direct Tributary use. | 2026-10-10 or next release, whichever comes first |
| [`RUSTSEC-2026-0173`](https://rustsec.org/advisories/RUSTSEC-2026-0173) (`proc-macro-error2`) | `sea-orm 1.1.20 -> sea-bae 0.2.1` | Informational/unmaintained compile-time macro dependency. Track SeaORM's removal or evaluate the SeaORM 2 migration. | 2026-10-10 or next release, whichever comes first |
| `spin 0.9.8` **yanked** (added 2026-07-13) | `flume 0.11.1 -> sqlx-sqlite`, and `flume 0.12.0 -> mdns-sd 0.20.1` | Yanked release, not a vulnerability. Both paths are transitive through `flume`; Tributary cannot bump `spin` directly. Track a `flume` release that moves off the yanked version. | 2026-10-10 or next release, whichever comes first |
| [`RUSTSEC-2023-0071`](https://rustsec.org/advisories/RUSTSEC-2023-0071) (`rsa`, Marvin Attack) | in `Cargo.lock`, **never compiled** | Suppressed by `ignore` in `.cargo/audit.toml`. Verified 2026-07-14: `rsa` is reachable only through sqlx's optional `mysql` feature, which SeaORM does not enable — `cargo tree -i rsa` is empty and `sqlx-mysql` is absent from the built graph. `cargo audit` resolves against the lockfile rather than the compiled graph, so it reports the advisory regardless; the ignore exists to reflect that gap. It is **load-bearing, not vestigial** — removing it fails the CI audit job (checked). If a MySQL feature is ever enabled the ignore must be deleted first, because `rsa` then becomes real code. | 2026-10-10 or next release, whichever comes first |

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
- [x] Record implementation: commit `eb5458f` plus review follow-ups; 18 focused origin,
  redirect, Referer, header, redaction, userinfo, DAAP, and MPD tests.

### P1.5 Enforce response limits while streaming

- [x] Replace `Content-Length`-only checks with counted streaming reads.
- [x] Apply caps to API JSON, DAAP, authentication, radio, and album-art responses.
- [x] Add overall deadlines in addition to idle timeouts.
- [x] Test missing, false, and oversized `Content-Length`, plus endless chunked bodies.
- [x] Record implementation: commit `842341b`; 14 focused counted-size, deadline,
  timeout-classification, allocation-boundary, URL-redaction, and backend-mapping tests.

### P1.6 Stop handing backend credentials to receivers

**This is the highest live exposure in the tracker.** It is not limited to "broad bearer
tokens": with Subsonic's plaintext auth mode the URL carries `p=enc:<hex_password>` — the
user's actual password, hex-encoded and trivially reversible. Unlike a token, a password
cannot be revoked, and users reuse it. Playing one track to a Chromecast is enough.

Confirmed path, end to end:

| Step | Location |
|---|---|
| Credential is baked into the stream URL | `plex/client.rs:236-242` (`X-Plex-Token`, account-wide), `jellyfin/client.rs:256-260` (`api_key`), `subsonic/client.rs:195-198` (`p=enc:<hex_password>`) |
| Retained in the generic model | `architecture/models.rs:60` — `Track.stream_url` |
| Copied verbatim into the UI object | `ui/window.rs:2080` |
| Handed to the receiver untouched | `audio/chromecast_output.rs:1406-1409` — `resolve_uri` proxies **only** `file://`; every `http(s)://` URL is returned as-is and becomes the Cast `content_id`. MPD sends the same URL via `addid` over **plaintext TCP**, so it also lands in the daemon's queue state and `mpd.log`. |

Design — generalize the existing local-file server into a media ticket proxy. The building
block is already sound: `audio/cast_http_server.rs` binds a LAN-only listener on port 0 and
routes on an unguessable v4 UUID (`:120-133`). What it lacks is upstream fetching and
revocation.

- [x] Give the proxy two registration kinds: `MediaSource::Local(PathBuf)` (today's behavior) and
  `MediaSource::Upstream(Url)`, where the proxy — not the receiver — fetches from the backend
  using an app-owned client bound by the P1.4 exact-origin policy. Only `Range` is forwarded
  upstream, and the target URL is fixed at registration, so the proxy cannot be driven to fetch
  anything else.
- [x] Issue receivers an opaque ticket URL. The device sees
  `http://<lan-ip>:<port>/cast/<uuid>` and never a credential.
- [x] Make the ticket registry insert **and revoke**. Registering a new upstream revokes the
  previous one, so at most one credential-bearing ticket is live; `stop()` revokes them all.
  Revocation deliberately does **not** happen on pause or seek — a Cast device re-fetches with a
  `Range` header when it seeks, so a ticket must outlive those.
- [x] Route **Chromecast** through it (`classify_cast_uri`). Unauthenticated streams (internet
  radio) still pass through directly: there is no secret to protect and relaying a live radio
  stream through this process would buy nothing.
- [x] Threat-model spoofed and compromised LAN receivers: a hostile receiver now obtains a
  single-media ticket, revoked on stop and superseded on the next load, instead of an account
  credential.
- [ ] Route **MPD** through the same proxy. MPD still sends the credential-bearing URL via `addid`
  over **plaintext TCP**, so it crosses the LAN in the clear and lands in the daemon's queue state
  and `mpd.log`. Blocked only by sequencing: `audio/mpd_output.rs` is owned by P1.8 (PR #76). The
  proxy it needs already exists — this is a small change once that lands.
- [ ] Keep backend credentials outside generic `Track` values: drop the credential from
  `Track.stream_url` and add a backend `resolve_stream(&track) -> (Url, HeaderMap)` called at
  playback time, moving `X-Plex-Token` / `api_key` / Subsonic `u,t,s,p` out of the query string
  and into request headers held only inside the proxy. The proxy makes the *leak* stop; this makes
  the credential stop being copied through the UI layer at all. Build it to serve P3.1's
  "resolve playable URLs at playback time" as well, rather than twice.
- [ ] Give tickets a TTL in addition to revocation, so a ticket cannot outlive a crashed session.
- [x] Record implementation: Chromecast proxy in commit `c6aa7df`; 6 focused classification,
  credential-detection, and pass-through tests. MPD and credential-free `Track` values remain open.

Acceptance criteria: no credential belonging to a remote backend is ever transmitted to a
device or daemon Tributary does not own. **Chromecast now meets this; MPD does not yet.** Closing
the MPD half also closes P1.4's last checkbox.

### P1.7 Serialize Chromecast lifecycle and commands

- [x] Use one ordered worker/session for load, play, pause, seek, volume, and stop.
- [x] Check cancellation before each external side effect and emitted event.
- [x] Prevent stale loads from launching or replacing newer media.
- [x] Ensure every failure terminates in a coherent state, never Error then Buffering.
- [x] Add delayed-device and supersession tests.
- [x] Record implementation: commit `60ee2af`; 24 focused FIFO, delayed-device,
  supersession, cleanup-retry, terminal-state, polling-fairness, and redaction tests.

### P1.8 Implement authoritative MPD state

- [ ] Serialize MPD commands through one worker or persistent connection.
- [ ] Emit Playing, Paused, Stopped, position, duration, and completion events.
- [ ] Clear buffering timers on success and error.
- [ ] Redact authenticated URLs from command logs.
- [ ] Add fake-server tests including slow and reordered commands.
- [ ] Record implementation: _pending_

### P1.9 Prevent stale async source rendering

Re-scoped 2026-07-13 after auditing the actual call sites. The original wording implied
playlist, radio, and remote loads were all unguarded. They are not — most already hold the
active-key guard, and only the two radio loads are genuinely exposed:

| Load | Location | Guarded? |
|---|---|---|
| Playlist / smart playlist | `ui/source_connect.rs:209` | yes |
| USB device | `ui/source_connect.rs:350` | yes |
| Remote sync (Subsonic/Jellyfin/Plex/DAAP) | `ui/window.rs:1795`, `:1854` | yes |
| Disconnect | `ui/discovery_handler.rs:152`, `:182` | yes |
| Radio Top Clicked / Top Voted | `ui/source_connect.rs:434-448` | **no** — `display_tracks` fires unconditionally; `active_source_key` is not even cloned into the closure |
| Radio Stations Near Me | `ui/radio.rs:331-345` | **no** — `fetch_and_display_nearme` does not take `active_source_key`, so it structurally cannot guard |

`ui/browser.rs` and `ui/playlist_actions.rs` need no guard: neither performs an async load that
reaches `display_tracks`.

- [x] Refresh already-open playlist URIs after an ID-preserving local rename and overlay committed
  URIs onto an in-flight result before publication.
- [x] Guard the two radio loads with the same active-key check the playlist and USB loads already
  use. `fetch_and_display_nearme` now takes `active_source_key` and checks it before rendering, and
  the Top Clicked / Top Voted closure clones and checks it too. Clicking away from a slow radio
  fetch no longer replaces the library view with stations while the sidebar still says Local.
- [ ] Promote the guard from a bare source **key** to a key plus **generation**: re-selecting the
  same playlist twice leaves two in-flight loads with identical keys, so the older one can still
  render last. This is the residual recorded in the 2026-07-12 decision note.
- [ ] Reload an active playlist after watcher reconciliation remints or relinks track IDs.
- [ ] Cache completed results even when no longer active.
- [ ] Add navigation-race tests, including same-key re-selection.
- [ ] Record implementation: _pending_

Acceptance criteria: a late async result never renders into a source the user has already
navigated away from, and never replaces a newer result for the same source.

### P1.10 Make foreign-key enforcement explicit

Found 2026-07-13 while auditing P1.1. SQLite defaults `foreign_keys` to **off**. Nothing in
Tributary turned it on: `db/connection.rs` set only `journal_mode` and `busy_timeout`, and
SeaORM's SQLite connector never touches the pragma (it parses the URL into
`SqliteConnectOptions` and applies only logging and pool settings). The `ON DELETE SET NULL`
that all of P1.1 was built to deliver was therefore working purely because sqlx happens to
default the pragma on. A change to that upstream default would not have failed a test — it
would have silently reverted P1.1 to dangling `track_id` values.

- [x] Set `foreign_keys`, `journal_mode`, and `busy_timeout` on `SqliteConnectOptions` so they
  apply to every pooled connection, not just the first one borrowed at startup.
- [x] Cover it with a test that fails if the pragma is ever lost: assert the pragma on several
  concurrently-held pooled connections, and assert that deleting a track nulls its playlist
  entry rather than orphaning a dangling ID.
- [x] Record implementation: commit `1c31b52`; 2 focused pooled-connection and cascade tests.

Acceptance criteria: playlist-entry integrity does not depend on an upstream default.

## P2 — Resilience, data semantics, and packaging

### P2.1 Correct smart-playlist semantics

- [x] Parse dates/timestamps instead of comparing strings. `eval_date` compared a track's RFC3339
  instant against a rule's bare `YYYY-MM-DD` as raw strings. `"2025-06-15T10:30:00+00:00"` is never
  equal to `"2025-06-15"`, so **"Date Added *is* X" matched zero tracks, forever**. `IsAfter` had
  the mirror-image bug: the longer string sorts greater than its own date prefix, so a track added
  *on* the boundary day counted as "after" it. Both sides are now parsed.
- [x] Define date-only versus instant behavior and timezone rules. A track timestamp is an
  **instant**; a rule date is a **calendar day** interpreted as the half-open UTC range
  `[00:00, next 00:00)`. `Is` means "falls within that day", `IsAfter` means "after the whole of
  that day". An unparseable instant or rule date fails to match rather than matching everything.
- [x] Validate relative-date amounts and use checked arithmetic. `Duration::days(i64::from(amount)
  * 30)` on an editor-supplied `u32` could push the subtraction past chrono's representable range
  and panic; the window is now computed with `checked_mul` + `try_days` + `checked_sub_signed`, and
  a window too large to represent matches everything instead of blowing up.
- [x] Add date tests that use the shape production actually stores. The old tests passed *date-only*
  strings on both sides — a shape the database never produces — which is precisely why these bugs
  survived. 10 focused tests now cover day containment, both boundary days, offset normalization,
  unparseable input, relative windows, and the overflow case.
- [ ] Select/truncate by limit criteria before applying final compound sort.
- [ ] Implement snapshot behavior for `live_updating = false` or remove the option.
- [ ] Add combined rule/limit/sort tests.
- [ ] Record implementation: date semantics in commit `93f6772`; 10 focused tests. Limit-versus-sort
  interaction and `live_updating` remain open.

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

Re-scoped 2026-07-13. The temp-file-plus-rename path the review implied was missing **already
exists** (`local/tag_writer.rs:81-106`). The real defects are narrower and all reachable from
right-click → Properties → Save (`ui/properties_dialog.rs:302`, `:385-400`):

- [x] Validate numeric edits before rewriting the file. Year, track, and disc each used
  `else if let Ok(n) = …parse::<u32>()` with **no `else` branch**, so an unparseable value was
  silently discarded: typing `2026a` into Year rewrote the file, bumped its mtime, changed
  nothing, and reported success. `TagEdits::validate` now rejects the whole edit before any file
  is opened, the dialog surfaces it while the user can still fix it, and the numeric entries
  declare `InputPurpose::Digits`.
- [x] Guarantee cleanup on every failure path. A failed `fs::rename` escaped through `?` from
  inside the success arm, orphaning `<song>.mp3.tributary-tag-tmp` next to the user's music
  forever. The temp file is now owned by an RAII guard that removes it unless it is explicitly
  persisted, so a failed rename cleans up too.
- [x] Use an exclusively created random sibling temp path. The fixed `.tributary-tag-tmp` suffix
  created via `fs::copy` had no `O_EXCL` and no randomness, so two concurrent saves to the same
  file clobbered each other and the copy would follow a symlink planted at the predictable path.
  Temp files are now created with `create_new` (`O_EXCL`) under a random UUID name.
- [x] Apply or remove the declared album-artist edit. `TagEdits.album_artist` existed and counted
  toward `is_empty()`, but `write_tags_to` never read it — the file was rewritten and the field
  ignored. It is now applied via `ItemKey::AlbumArtist`. (Still no widget sets it; implementing it
  removes the trap for whoever adds one.)
- [x] Preserve permissions and define durability/fsync behavior accurately. Permissions are copied
  from the file being replaced, and the tagged copy is `fsync`ed before the rename, so a crash
  cannot leave a truncated file where the original was. The module doc now states this rather than
  implying it.
- [x] Add injected-failure tests: 6 focused tests cover validation, a malformed number leaving the
  file byte-for-byte untouched with no temp behind, temp cleanup after a failed tag write,
  unsupported formats, and temp-path uniqueness plus self-removal. Concurrency is covered by the
  exclusivity property rather than by racing two threads.
- [ ] Add a happy-path fixture test. Every test above asserts behavior *before* lofty succeeds or
  *when* it fails, because they use files that are named like audio but are not decodable. Nothing
  yet asserts that a valid edit actually lands in a real MP3/FLAC — that needs a committed audio
  fixture and belongs with P3.5's coverage work.
- [x] Record implementation: commit `6d0ec95`; 6 focused tests.

### P2.4 Make removable-media browsing safe and asynchronous

Re-scoped 2026-07-13. `device/usb.rs` performs **no traversal at all** — it only `read_dir`s the
mount roots (`:64-87`), so the symlink defect is not there. The traversal lives in the UI.

- [x] Disable symlink following during device scans. The walk used
  `WalkDir::new(mount_path).follow_links(true)`, so a USB stick containing `music -> /home/user`
  made Tributary walk the entire home directory and index it as "on the device". The traversal is
  now extracted into `enumerate_device_audio_files`, uses `.follow_links(false)`, and tests
  `entry.file_type()` rather than `Path::is_file()` — the latter follows the link anyway and would
  still have pulled in an individual file symlinked from off the device. This matches the library
  scanner's policy (`local/engine.rs:772`).
- [x] Verify descendants remain on the selected mount/device, for the symlink case: nothing outside
  the mount can now be reached through a link. (A bind mount or a nested real filesystem under the
  mount point is still followed; the library scanner's `filesystem_boundary_id` check is the model
  to copy if that matters here.)
- [ ] Move mount **discovery** off the GTK thread. `ui/window.rs:71` calls `detect_usb_devices()`
  synchronously inside `build_window`. It is only a shallow `read_dir` + `is_dir()`, but against a
  hung mount (stale NFS, yanked stick) `is_dir()` blocks in the kernel and the app hangs at
  startup before the window is drawn. The *traversal* is already off-thread, so that half is done.
  Left open deliberately: it reorders sidebar population at startup and wants a live UI review.
- [ ] Use platform mount/volume APIs rather than directory heuristics.
- [x] Add malicious symlink tests: a device tree with both a directory symlink and a file symlink
  pointing off-device yields only the files physically on the device.
- [ ] Add stale-mount and duplicate-device tests.
- [ ] Record implementation: symlink containment in commit `1886847`; 2 focused traversal tests. Mount
  discovery and platform volume APIs remain open.

### P2.5 Repair Flatpak behavior and local build path

- [ ] Put the pinned generator where local and CI builds both use it.
- [ ] Generate `build-aux/flatpak/cargo-sources.json` consistently.
- [ ] Define narrow USB/removable-media permissions or a portal workflow.
- [ ] Define writable custom-library behavior for tag editing.
- [ ] Run a local Flatpak build and smoke-test USB/custom-library behavior.
- [ ] Record implementation: _pending_

### P2.6 Synchronize packaging metadata

- [x] ~~Fix RPM `Version`, `Source0`, and `%autosetup` naming.~~ **Withdrawn 2026-07-14 — this was
  a false finding.** The in-repo `Version: v0.1.0` is a placeholder that Packit overwrites from the
  release tag at build time, so COPR ships the correct version; v0.5.0 built and released through
  this path. `build-aux/arch/PKGBUILD` is likewise rewritten at release time
  (`release.yml:500` seds `pkgver`). The checked-in literals are stale to a *reader* but never
  reach a package. Nothing to fix. Recorded rather than deleted, so the same wrong conclusion is
  not re-derived from reading the spec in isolation.
- [ ] Raise GTK runtime minimum to 4.16.
- [ ] Raise libadwaita runtime minimum to 1.6. Both minimums are currently under-declared as
  `>= 4.14` / `>= 1.5` in `build-aux/rpm/tributary.spec:23-24`, in Cargo.toml's
  `generate-rpm.requires`, and in Cargo.toml's `deb.depends`, against a crate that pins `v4_16`
  and `v1_6` (`Cargo.toml:12-13`). The shipped `.deb`/`.rpm` therefore install onto systems where
  the binary cannot start.
- [ ] Add `%U` or `%F` to the desktop `Exec` line (`data/io.github.tributary.Tributary.desktop:6`
  is bare `Exec=tributary`). Until this lands, the "Linux file association" feature CHANGELOG
  advertises for 0.5.0 does not work: the `MimeType` entry is present and the binary handles
  opens, but the desktop entry never passes the URI.
- [ ] Add the required `AudioVideo` desktop category.
- [ ] Add the 0.5.0 AppStream release entry (newest is 0.4.1).
- [ ] Update README Rust requirement from 1.80 to 1.85.
- [ ] Add CI on the declared Rust 1.85 MSRV. Every toolchain in CI is
  `dtolnay/rust-toolchain@stable`; nothing verifies that 1.85 still compiles.
- [ ] Enforce the global validation gate in CI. It is currently a *local* gate: CI runs
  `cargo test --release` only (`ci.yml:83`), never `cargo test --all-targets`, so a break that
  only appears in debug (`debug_assert!`, overflow checks) ships. Nothing runs `appstreamcli` or
  `desktop-file-validate` in any workflow, and `fuzz/` is its own workspace so neither `fmt` nor
  `clippy` ever covers it.
- [x] Apply a redirect policy to the app's non-credential HTTP clients. Radio-Browser
  (`radio/client.rs`), IP geolocation, and MusicBrainz (`ui/properties_dialog.rs`) bypassed
  `http_security` entirely and ran reqwest defaults, so they sent a `Referer` and would follow an
  HTTPS→HTTP downgrade. They now use a shared public policy that still permits the cross-host
  mirror redirects those services depend on but refuses to be walked down to plaintext.
- [ ] Record implementation: non-credential redirect policy in commit `8368a65`; packaging remains
  open.

### P2.7 Fix platform cache paths

- [ ] Store GStreamer registries in a per-user cache directory.
- [ ] Avoid writes inside `/Applications` and Program Files.
- [ ] Generate or patch the macOS pixbuf loader cache for the installed absolute bundle path.
- [ ] Verify macOS signature integrity after first launch.
- [ ] Record implementation: _pending_

### P2.8 Bound Chromecast control I/O

- [ ] Adopt an upstream `rust_cast` timeout/custom-stream API or maintain a narrowly audited fork.
- [ ] Enforce connection, read, and write deadlines without moving a live non-`Send` Cast session
  between threads.
- [ ] Add a silent-receiver test proving a no-reply operation cannot pin Stop, replacement Load,
  or Shutdown forever.
- [ ] Record implementation: _pending_

### P2.9 Repair the AirPlay fallback path

Filed 2026-07-13. This is review finding **M3** (`CODE_REVIEW_2026-07-10.md:210-218`), which was
never given a tracker item — the only AirPlay references in this file are about routing streams
through the P1.6 proxy, which is a different problem.

The fallback is architecturally backwards, and it is the *default* path on a typical Linux box:
`raopsink` ships in `gst-plugins-bad` and is absent on most distributions, so
`airplay_output.rs:172-175` routes to `build_shairport_pipeline` in the common case. That
function opens by discarding the receiver the user selected —
`airplay_output.rs:289-295`: `let _ = (host, port); // shairport-sync uses its own discovery.` —
and then pipes PCM into `shairport-sync`, which is an AirPlay **receiver**, not a sender. It
cannot transmit to the device that was clicked.

- [ ] Either transmit to the *selected* receiver, or remove the fallback and surface an
  actionable "install `gst-plugins-bad` for AirPlay support" error instead of silently spawning a
  subprocess that cannot work.
- [ ] Move the `which` probe (`airplay_output.rs:298`), the subprocess spawn, and teardown off the
  GTK main thread; they run synchronously under `load_uri` today.
- [ ] Add a test proving a missing `raopsink` produces an actionable error rather than a silent
  no-op stream.
- [ ] Record implementation: _pending_

Acceptance criteria: selecting an AirPlay receiver either plays to that receiver or fails with an
error that tells the user what to install.

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

**This gate is local, and CI does not enforce all of it.** Checked boxes mean the step was run by
hand before a milestone, not that a regression would be caught automatically. As of 2026-07-13 CI
runs `cargo fmt --check`, both `clippy -D warnings` invocations, `cargo test --release`, and
`cargo audit` — but **not** `cargo test --all-targets` (so a debug-only break ships), and no
workflow runs `appstreamcli` or `desktop-file-validate` at all. `fuzz/` is a separate workspace
and is covered by neither `fmt` nor `clippy`. Closing that gap is tracked under P2.6.

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
- 2026-07-12 — P1.5 treats every finite HTTP response as an observed byte stream rather than
  trusting `Content-Length`: Subsonic, Jellyfin, Plex, DAAP, authentication, radio/geolocation,
  artwork, and MusicBrainz reads now stop at endpoint-specific caps and carry end-to-end request
  deadlines in addition to idle-read timeouts. Async and blocking collectors classify request
  timeouts consistently, discard credential-bearing request URLs from retained body errors, and
  fail cleanly on impossible or unavailable allocations. Audio and live-radio media streams remain
  deliberately uncapped because they are length-unbounded playback transports; credential-bearing
  playback still belongs to the cancellable P1.6 proxy/ticket design.
- 2026-07-12 — P1.7 places the non-`Send` Cast device, application, media session, controls,
  heartbeat, status polling, and teardown on one FIFO worker. Epoch checks bracket every Cast
  effect and event; ownership is recorded immediately after application launch and media load so
  superseded calls remain cleanable. Failures retire the session before publishing Stopped then
  Error, clean media/application ownership with fair bounded retries, and abandon an unreachable
  application after three attempts so a replacement load can reconnect. The legacy local-file
  resolver remains synchronous until P1.6 replaces it with the shared proxy/ticket path.
  `rust_cast 0.21` uses blocking `TcpStream` calls, hides the socket, and offers no timeout or
  custom-stream constructor, so cancellation can be checked only before and after an in-flight
  call; hard receiver I/O deadlines require an upstream change or audited fork and are tracked as
  P2.8 rather than overstated as part of P1.7.

- 2026-07-13 — Documentation audit against the committed tree. Every `[x]` in P1.1–P1.7 was
  verified against the source and none was overstated. The drift was everywhere else: CHANGELOG
  had recorded none of the remediation, README still advertised Rust 1.80 and a `MediaBackend`
  abstraction that is never constructed (`grep -rn "dyn MediaBackend" src/` returns nothing), and
  one review finding (M3, AirPlay) had no tracker item at all. P1.9, P2.3, and P2.4 described
  defects that were real but lived in different files than the tracker claimed, so each was
  re-scoped in place with the actual call sites rather than left to mislead an implementer.
- 2026-07-13 — Foreign-key enforcement is now stated rather than inherited (P1.10). SQLite
  defaults `foreign_keys` off; sqlx defaults it on; SeaORM never touches it. P1.1's entire
  `ON DELETE SET NULL` guarantee rested on that middle link. Removing the new pragma makes the
  added tests fail with a *dangling* `track_id` pointing at a deleted track, which is precisely
  the corruption P1.1 exists to prevent.
- 2026-07-13 — Non-credential HTTP clients get their own redirect policy rather than the
  authenticated one. Radio-Browser, geolocation, and MusicBrainz legitimately redirect across
  hosts, so exact-origin would break them; they instead refuse HTTPS→HTTP downgrades and send no
  `Referer`. `RadioBrowserClient::new` now returns `Result` instead of degrading to
  `reqwest::Client::default()` on a builder failure — that "fallback" carried neither a timeout
  nor a redirect policy, and `Client::default()` panics on the same TLS-init failure that would
  have triggered it, so it could never have been a safety net.

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
| 2026-07-12 | P1.5 | `842341b` | Counted finite-response reads enforce endpoint caps and total deadlines across API, authentication, DAAP, radio, artwork, and metadata clients. |
| 2026-07-12 | P1.7 | `60ee2af` | One worker owns the Cast session and serializes effects, authoritative state, cancellation, cleanup retries, and stale-event suppression. |
| 2026-07-13 | P1.10 | `1c31b52` | Foreign keys, WAL, and busy timeout are set on every pooled connection instead of inherited from an sqlx default; 2 tests fail loudly if the pragma is ever lost. |
| 2026-07-13 | P2.6 (partial) | `8368a65` | Radio-Browser, geolocation, and MusicBrainz clients now refuse HTTPS→HTTP redirect downgrades and send no `Referer`; packaging metadata remains open. |
