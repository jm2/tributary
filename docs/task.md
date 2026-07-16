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
- Before opening any remediation PR, update both this tracker and `CHANGELOG.md` on the branch.
  Every user-visible fix must be described in that PR. The remediation work through P1.7 landed
  without those updates, and the changelog silently drifted four months behind the code; a user
  could not tell that the migration corruption or the destructive reconciliation had ever been
  fixed.

Status summary:

- [ ] P0 release blockers complete
- [x] P1 correctness and security complete
- [ ] P2 resilience and packaging complete
- [ ] P3 architecture and integration coverage complete

Progress snapshot (2026-07-16), recounted from the literal P0–P3 task checkboxes to correct the
earlier numerator/denominator drift. The live protected-playback finding recorded under P2.11 now
has five independently verifiable tasks rather than the original three compound boxes. The
denominator excludes the two deferred P0.7
live-workflow verification boxes and the withdrawn P2.6 false finding; section-summary and
global-validation gate boxes are not task progress:
**173/220 (78.6%)** in-scope checklist items complete: **50/50 P0**, **64/64 P1**, **56/76 P2**,
and **3/30 P3** after those exclusions. This incorporates the four P2.9 boxes closed by PR #99
and the seven remaining P2.6 boxes closed by PR #100 since the prior snapshot. The release-workflow
dry run remains deliberately deferred rather than being counted as unfinished P0 remediation.

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
  roots, plus marker corruption/duplication/conversion and watcher-cache invalidation. PR #68's
  44 safety-core tests are supplemented by 31 focused explicit-trust engine/UI tests covering
  inherited and replacement roots and trust requests whose complete observation has no supported
  audio; dismissal and unknown responses; stale evidence, compare-and-swap, marker, and mount
  drift; read-only marker-create failure and valid-marker adoption; retry/idempotency; a
  non-destructive conversion followed by a distinct ordinary scan; and command processing without
  a filesystem watcher. A further 26 focused retained-authority tests cover same-content marker
  replacement; same-marker root replacement; bound-file, bound-directory, and bound-absence
  evidence; missing-ancestor recreation and retained-parent replacement; descendant and ancestor
  replacement even when a hard-linked final object keeps the same identity; mount/filesystem
  boundary rejection; path escape and symlink traversal; transactional rollback of root-state
  promotions, upsert, deletion, and rename changes; playlist-link preservation; success-event
  suppression;
  Windows namespace pins that block root, marker, file, and directory rename/deletion only while
  their retained handles live; and blocking-task failure rollback without false root demotion.
  One direct end-to-end watcher-backlog/confirmation ordering harness remains future integration
  coverage; the current ordering is exercised through engine-loop and control-flow unit
  components.
- [x] Add an explicit trust/re-enrollment flow for roots inherited from a pre-identity database,
  confirmed roots whose identity changed, and unconfirmed trust requests whose complete
  observation has no supported audio files. Tributary now queues exact configured-root prompts
  one at a time in the main window, never accepts content similarity as identity proof, presents
  replacements as destructive, and requires a second acknowledgement for every such
  no-supported-audio trust request. Filesystem evidence remains private to the engine and is
  revalidated with persisted-state compare-and-swap and fresh identity/mount checks before consent
  can change authority. A brand-new writable root whose first complete observation contains
  supported audio and has no remembered metadata continues to enroll automatically; once an empty
  observation has been recorded, later content still requires consent because Tributary cannot
  distinguish newly added files from a removable or network volume appearing at the mountpoint.
- [x] Pin authority-promoting root-state changes, reconciliation, and watcher mutations to a
  retained root-and-marker lease on Unix and Windows. Each marker-backed root capable of
  authorizing mutations retains one lease for its initial scan or watcher batch. Mutation-bearing
  files, directories, and missing names are resolved beneath that lease without following
  symlink/reparse-point components or crossing a different mount/filesystem boundary; the same
  lease and descendant evidence are revalidated after SQL changes and immediately before commit.
- [x] Record implementation: the safety core and review follow-ups are in PR #68 with 44 focused
  tests; explicit trust/re-enrollment is in `aecbce6` with 31 additional focused tests; retained
  root authority is in `ed0a300`, with review and CI follow-ups in `7704db8`; together they add 26
  focused tests.

Acceptance criteria: Tributary never deletes persisted track metadata based on an incomplete
view of a library root. An inherited or replaced root requires explicit confirmation, as does any
trust request whose complete observation has no supported audio files. Those requests require a
complete marker-backed conversion and a distinct complete ordinary scan before becoming
authoritative. The conversion changes root authorization but performs no track upserts or
deletions; the ordinary scan may reconcile immediately afterward, with no grace period.
Declined, stale, failed, and incomplete decisions remain unavailable and preserve remembered
metadata, while intentional offline deletion is eventually reflected after authority is active.
Every root-state promotion and track upsert, deletion, or rename is justified by one retained
root/marker lease and descendant evidence opened beneath it. If that lease or evidence changes
before final in-transaction validation, the mutation rolls back and publishes no success event.

### P0.3 Preserve DAAP session lifetime

- [x] Retain the connected backend/session for as long as the source is connected.
- [x] Remove logout network activity from `DaapBackend::drop`.
- [x] Populate and/or replace the explicit disconnect path with owned backend shutdown.
- [x] Resolve stream/artwork from the live session at playback time.
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
- [x] Record implementation: PR #68; 25 focused UI/output tests. A 2026-07-15 live Windows DAAP
  check exposed a separate idle-Play abort: the immutable `RefCell` borrow used to choose
  `StartAt` lived through the `match` arm that installed the new queue. PR #92 resolves the
  request behind a function boundary before dispatch; the existing Stop-then-Play regression now
  uses the real `RefCell` boundary and proves the `StartAt` result permits immediate mutable queue
  replacement.

Acceptance criteria: view mutations never change the identity of the playing track or the
meaning of queue navigation. Starting playback from an idle non-empty view releases every session
read borrow before installing the new queue.

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
- [x] Record implementation: PR #68 supplied the initial dependency fixes and then-known warning
  dispositions; commits `a35cde8` and `e9a3efc` document the follow-up and update `spin`.
- [x] Re-close the disposition table after its post-2026-07-10 drift (found and corrected
  2026-07-13). After updating `spin`, `cargo audit --no-fetch` reports exactly two allowed
  warnings, both unmaintained dependencies. The separate `RUSTSEC-2023-0071` ignore is
  justified and time-bounded below rather than removed based on active-tree output: the
  affected package remains in `Cargo.lock` even though it is inactive in Tributary's
  configured feature graph.

Audit disposition recorded 2026-07-10, amended and revalidated 2026-07-13:

| Advisory | Dependency path | Disposition | Revisit by |
|---|---|---|---|
| [`RUSTSEC-2026-0190`](https://rustsec.org/advisories/RUSTSEC-2026-0190) (`anyhow`) | direct and transitive | Fixed by locking and requiring `anyhow >= 1.0.103`. | Closed |
| [`RUSTSEC-2024-0436`](https://rustsec.org/advisories/RUSTSEC-2024-0436) (`paste`) | `lofty 0.24.0 -> paste 1.0.15` | Informational/unmaintained, with no patched `paste` release. Track Lofty migration to a maintained replacement; no direct Tributary use. | 2026-10-10 or next release, whichever comes first |
| [`RUSTSEC-2026-0173`](https://rustsec.org/advisories/RUSTSEC-2026-0173) (`proc-macro-error2`) | `sea-orm 1.1.20 -> sea-bae 0.2.1` | Informational/unmaintained compile-time macro dependency. Track SeaORM's removal or evaluate the SeaORM 2 migration. | 2026-10-10 or next release, whichever comes first |
| [`RUSTSEC-2023-0071`](https://rustsec.org/advisories/RUSTSEC-2023-0071) (`rsa`, Marvin Attack) | Lockfile-only optional graph: `sqlx 0.8.6` and `sqlx-macros-core 0.8.6` retain `sqlx-mysql 0.8.6 -> rsa 0.9.10`; `sqlx-mysql` and `rsa` are inactive under Tributary's `sqlx-sqlite` feature set. | Retain the narrowly documented `.cargo/audit.toml` ignore. `cargo tree --locked -i rsa` and `cargo tree --locked -e features -i sqlx-mysql` are empty, but `cargo-audit` checks every locked package and fails on this advisory without the ignore. No fixed upgrade exists. Re-review immediately before enabling MySQL support. | 2026-10-10 or next release, whichever comes first; immediately if MySQL is enabled |

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
- [x] Route credential-bearing local and AirPlay playback through an app-owned fetch path
  protected by the exact-origin policy. Each protected load now receives a dedicated opaque
  loopback ticket; Tributary performs the authenticated exact-origin/no-`Referer` fetch and
  forwards only `Range`, while credential-free radio, files, and library paths remain direct.
- [x] Strip credential-bearing URLs from every retained/formatted HTTP or pipeline error.
- [x] Stop logging raw DAAP session IDs and authenticated MPD commands.
- [x] Add redirect matrix tests using mock servers.
- [x] Record implementation: commit `eb5458f` plus review follow-ups; 18 focused origin,
  redirect, Referer, header, redaction, userinfo, DAAP, and MPD tests. Local/AirPlay protected
  playback boundary: commit `2188efb` with concurrency review follow-up `28e3400`; 10 focused proxy,
  redirect/header-isolation, stale-cleanup, lifecycle, and fail-closed tests.

### P1.5 Enforce response limits while streaming

- [x] Replace `Content-Length`-only checks with counted streaming reads.
- [x] Apply caps to API JSON, DAAP, authentication, radio, and album-art responses.
- [x] Add overall deadlines in addition to idle timeouts.
- [x] Test missing, false, and oversized `Content-Length`, plus endless chunked bodies.
- [x] Record implementation: commit `842341b`; 14 focused counted-size, deadline,
  timeout-classification, allocation-boundary, URL-redaction, and backend-mapping tests.

### P1.6 Stop handing backend credentials to receivers

This began as the tracker's highest live credential exposure. It is not limited to broad bearer
tokens: Subsonic's legacy compatibility mode ultimately needs `p=enc:<hex_password>` on the
upstream request — the user's password, hex-encoded and trivially reversible. The completed design
keeps that material, Subsonic token/salt authentication, the Plex and Jellyfin tokens, and DAAP's
bearer session ID out of generic catalogue and GTK values as well as every receiver. Playback
credential material stays inside the retained backend client/resolver and is materialized for
media only in the app-owned proxy's immediate exact-origin upstream request.

Confirmed path, end to end:

| Step | Location |
|---|---|
| Generic catalogue | Subsonic, Jellyfin, and Plex tracks keep `stream_url` and `cover_art_url` empty. Their backend caches retain backend-native stream locators and track-only artwork locators keyed by deterministic app track UUID, so a type-local album/artist ID cannot overwrite track art. DAAP continues to publish its already-credential-free live-session references. |
| Source ownership | `source_registry.rs` retains an `Arc<dyn RemoteMediaResolver>` behind an exact source generation, random lease UUID, and revocable `MediaLease`. A replacement, release, discovery removal, manual deletion, or shutdown invalidates old references and already-resolved requests. DAAP retains its stateful session in its existing generation-scoped registry and now attaches the same revocable-request guarantee to media issued by that session. |
| GTK publication | A current standard-remote sync is converted to `tributary-remote://<lease>/{stream,artwork}/<track-uuid>`. The reference contains no source address, backend-native ID, or credential; a queued sync is rejected unless its generation and lease still own that source. |
| Playback and artwork | `ui/playback.rs` resolves the exact opaque standard or DAAP reference only when the item is consumed. Playback generations reject a result completed after Stop, Next, output replacement, or a newer replay; artwork repeats generation and lease checks before and after its worker fetch. |
| Credential isolation | `ResolvedHttpRequest` is deliberately non-debuggable and non-serializable. Plex uses a sensitive `X-Plex-Token` header and Jellyfin a sensitive `X-Emby-Authorization` header. Subsonic protocol authentication remains private query material (`u` plus `t`/`s` or HTTPS-only `p`), and DAAP's bearer `session-id` is now private query material too; each is appended only inside the app-owned proxy immediately before the exact-origin fetch. |
| Output boundary | `AudioOutput::load_resolved` accepts the typed request. Chromecast, MPD, local GStreamer, and AirPlay exchange it for their existing opaque, receiver-reachable tickets; none can fall back to the clean endpoint or serialized credential state. |

The opaque source reference and the output ticket are separate capabilities. The first identifies
one track through one live app session and is useful only inside Tributary. Resolution produces a
typed, revocable HTTP request, and only then does the selected output mint a ticket reachable by
its receiver. Chromecast keeps its LAN IPv4 listener; MPD binds to the successful connection's
local IP/address family; local `playbin3` and AirPlay `uridecodebin` use dedicated loopback-only
proxies. All ticket routes use OS-assigned ports and unguessable UUIDs.

- [x] Give the proxy local and upstream registration kinds. Local routes retain a bound path;
  upstream routes accept the legacy/direct `Url` or the current typed `ResolvedHttpRequest`, and
  the proxy — not the receiver — fetches from the backend using an app-owned client bound by the
  P1.4 exact-origin policy. Only `Range` is forwarded upstream, and the endpoint/auth state is
  fixed at registration, so the proxy cannot be driven to fetch anything else.
- [x] Issue receivers an opaque ticket URL. The device sees
  `http://<bound-ip>:<port>/cast/<uuid>[.<audio-ext>]` (with IPv6 bracketed) and never a
  credential. Chromecast uses a LAN IPv4 address; MPD uses the successful connection's local
  address and family.
- [x] Make the ticket registry insert **and revoke**. Registering a new upstream revokes the
  previous one, so at most one credential-bearing ticket is live; `stop()` revokes them all.
  Revocation deliberately does **not** happen on pause or seek — a Cast device re-fetches with a
  `Range` header when it seeks, so a ticket must outlive those within its hard lifetime.
- [x] Route **Chromecast** through it. Standard remote media enters through typed
  `load_resolved`; `classify_cast_uri` remains the fail-closed boundary for direct/legacy inputs.
  Unauthenticated streams (internet radio) still pass through directly: there is no secret to
  protect and relaying a live radio stream through this process would buy nothing.
- [x] Threat-model spoofed and compromised LAN receivers: a hostile receiver now obtains a
  single-media ticket whose route is revoked on stop, superseded on the next load, and denies all
  new lookups after 24 hours instead of an account credential.
- [x] Route **MPD** through the same proxy. Standard remote media uses typed `load_resolved`, while
  `MpdOutput::load_uri` still classifies every direct/legacy input before it can enter the ordered
  worker. A protected request remains app-private and is consumed only by the ordered worker/proxy.
  The worker starts a dedicated proxy on the successful TCP connection's exact local IPv4/IPv6
  route and sends only the opaque ticket via `addid`.
  Missing runtime/address state, unusable IPv6 scope, proxy startup,
  registration, and generated-argument failures all fail closed without falling back to the raw
  URL. A replacing direct, protected, or rejected load, user Stop, output drop, natural end,
  ownership loss, operation failure, worker shutdown, and stale generation revoke the ticket;
  pause, seek, and an explicit remote Stop that retains the same song keep it restartable only
  within the ticket's hard lifetime.
- Local and **AirPlay** playback now use the same upstream proxy at their GStreamer boundary.
  Each protected load owns a dedicated loopback listener and ticket, while direct media is passed
  through byte-for-byte. Missing runtime, bind/client/ticket validation, malformed declared
  HTTP(S), and credentialed unsupported schemes fail closed with fixed URL-free events. A
  replacement (including direct or rejected media), Stop, EOS, pipeline error, setup/preroll/start
  failure, output drop, or proxy drop revokes the route; pause, play, and seek retain it only
  within its hard 24-hour lifetime. Dedicated servers plus identity-checked cleanup ensure a
  delayed callback can retire only its own ticket, never a newer load.
- [x] Keep backend credentials outside generic `Track` and GTK values. Standard remote tracks no
  longer retain stream or artwork URLs; `RemoteMediaResolver::resolve_stream` and
  `resolve_artwork` translate a stable app track UUID through backend-private native locators only
  when playback/artwork is consumed. `ResolvedHttpRequest` separates the credential-free endpoint
  from Plex/Jellyfin sensitive headers, Subsonic's protocol-required private query pairs, and
  DAAP's bearer `session-id`. Only the app-owned exact-origin proxy materializes those fields.
  DAAP's sibling stateful registry now resolves through this same typed boundary and revokes
  already-issued requests on replacement, release, discovery loss, or shutdown. The
  generation/lease registry and opaque UI references implement the corresponding remote portion
  of P3.1 rather than creating a second resolver later.
- [x] Give credential-bearing upstream tickets a hard, absolute, non-sliding 24-hour TTL in
  addition to lifecycle revocation. A ticket is live only before its monotonic deadline; at the
  deadline an atomic lookup removes it and returns the same 404 as a revoked or unknown route.
  GET/Range requests, pause, seek, and remote state do not renew it. A request admitted before
  expiry may finish afterward, while every later lookup fails. This bounds missed cleanup while
  the app/server remains alive; process/runtime death already closes the listener. Local-file
  routes retain their existing server-lifetime contract because they front no backend credential.
- [x] Record implementation: Chromecast proxy in commit `c6aa7df` with 6 focused classification,
  credential-detection, and pass-through tests; MPD proxy in commit `e23efd8` with 18 new focused
  tests (10 worker/ticket lifecycle, 3 media classification, and 5 route/body-error tests); hard
  upstream-ticket expiry in commit `8735862` with 6 deterministic deadline, non-renewal,
  revocation/supersession, local-route, admitted-response, and 404-equivalence tests; the
  local/AirPlay GStreamer boundary is in `2188efb` with concurrency review follow-up `28e3400`
  and 10 focused tests. The playback-time resolver/source-lease slice is in PR #86: typed
  credential-isolated requests, backend-private native locators, exact generation/lease
  publication, async playback/artwork resolution, lease-aware output proxying, and 36 newly
  authored focused tests across request validation, all three standard backends, registry races,
  stale playback/artwork work, each output boundary, and pre-persistence server-URL rejection. A
  late review follow-up in PR #87 omits Plex tracks that have no non-empty media part instead of
  publishing an opaque reference that can only fail, and searches later media/part entries before
  declaring a track unplayable. Locator, bitrate, and format now come from the same selected media
  entry; 2 focused tests cover missing, empty, and later valid locators and metadata alignment.
  The P2.11 retained-route follow-up in PR #97 converts DAAP's live URL materialization into a
  typed, private-query, lease-bearing request without changing its credential-free catalogue
  reference.

Acceptance criteria: no credential belonging to a remote backend is ever transmitted to a
device or daemon Tributary does not own, retained in a generic catalogue/UI value, or serialized
through an output boundary. **Complete:** standard remote tracks and GTK rows contain only stable
identity plus opaque lease references; DAAP retains credential-free live-session references whose
bearer ID is isolated only in a typed, revocable request at consumption time; all protected
playback/artwork requests resolve through the current app-owned session and proxy;
and Chromecast, MPD, local GStreamer, and AirPlay receive only their scoped tickets. P1.4 and P1.6
are complete.

### P1.7 Serialize Chromecast lifecycle and commands

- [x] Use one ordered worker/session for load, play, pause, seek, volume, and stop.
- [x] Check cancellation before each external side effect and emitted event.
- [x] Prevent stale loads from launching or replacing newer media.
- [x] Ensure every failure terminates in a coherent state, never Error then Buffering.
- [x] Add delayed-device and supersession tests.
- [x] Record implementation: commit `60ee2af`; 24 focused FIFO, delayed-device,
  supersession, cleanup-retry, terminal-state, polling-fairness, and redaction tests.

### P1.8 Implement authoritative MPD state

- [x] Serialize MPD commands through one worker or persistent connection.
- [x] Emit Playing, Paused, Stopped, position, duration, and completion events.
- [x] Clear buffering timers on success and error.
- [x] Redact authenticated URLs from command logs.
- [x] Add fake-server tests including slow and reordered commands.
- [x] Record implementation: commits `eb0b9ca` and `fbaaa7f`; PR #76; 43 focused FIFO,
  persistent-session, protocol-boundary, authoritative-state, ownership-preflight,
  queue-preservation, foreign-successor, EOS, timeout, poisoned-stream, IPv6-resolution,
  mode-reset, and credential-redaction tests.

### P1.9 Prevent stale async source rendering

Re-scoped 2026-07-13 after auditing the actual call sites, then completed 2026-07-15 with one
navigation authority shared by playlist/smart-playlist, USB, radio, local debounce, remote
connection, disconnect, and forced-local transitions. Every navigation mints an exact
`{source_key, generation}` request. A completion is classified as superseded and ignored, the
newest completion for an inactive key and cached without rendering, or the exact current request
and both cached and rendered. This closes both cross-source races and the same-key re-selection
race that a bare active-key comparison could not distinguish.

- [x] Refresh already-open playlist URIs after an ID-preserving local rename and overlay committed
  URIs onto an in-flight result before publication.
- [x] Guard both radio load paths. Top Clicked / Top Voted and Stations Near Me now carry their
  exact navigation request through the fetch; Near Me carries it through the consent dialog too,
  so a stale response neither starts a fetch nor forces a source change. Clicking away from slow
  radio work no longer replaces the library view while the sidebar still names another source.
- [x] Promote the guard from a bare source **key** to an exact key plus monotonic **generation**.
  Re-selecting the same playlist advances its generation, so the older request can neither replace
  the newer cache entry nor render last. Playlist, USB, radio, local-debounce, pending-remote,
  disconnect, and forced-local callbacks all consult the shared navigation authority. When remote
  authentication owns a deferred intent, the prior visible source retains its own exact latest
  generation so its derived browser/status projection can stay fresh without accepting an older
  away-and-back callback.
- [x] Reload an active playlist after watcher reconciliation remints or relinks track IDs. The
  engine publishes a post-reconciliation `PlaylistProjectionsInvalidated` event during initial
  scan and after a watcher batch commits a track mutation and attempts reconciliation; the UI
  first invalidates every outstanding playlist request and cached projection, clears rows whose
  IDs may no longer be actionable, and reloads only when that exact playlist still owns the
  current navigation intent.
- [x] Cache completed results even when no longer active. Only the newest generation for a source
  may update its cache; an inactive result is cache-only, while rendering additionally requires
  the exact current request. A transient playlist query/database failure preserves the last valid
  cache and visible rows, while a confirmed missing playlist deliberately evicts them.
- [x] Add navigation-race tests, including same-key re-selection. Eight focused navigation tests
  cover inactive caching, same-key and reverse-order supersession, playlist invalidation,
  pending-remote intent, visible-local refresh during pending authentication, and local debounce
  away/back behavior; two engine tests cover initial-scan and watcher post-reconciliation
  invalidation ordering, including the reconciliation-error path.
- [x] Record implementation: PR #88; eight focused
  navigation tests plus two focused engine invalidation tests.

Acceptance criteria: a late async result never renders into a source the user has already
navigated away from, and never replaces a newer result for the same source. **Complete:** a
pending remote authentication/connection owns a distinct navigation intent even while the prior
source remains visible, so a playlist refresh, sidebar rebuild, background remote publication, or
stale connection callback cannot steal that intent or leave the sidebar and content out of sync;
the prior visible source can still refresh from its exact latest projection generation.

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
- [x] Select/truncate by limit criteria before applying final compound sort. Rules first define the
  candidate set; the limit's `selected_by` order then determines membership and capacity-prefix
  truncation; the optional compound sort finally orders only that selected subset for display.
- [x] Remove the nonfunctional `live_updating` option. It was persisted and shown in the editor but
  was never read: checked and unchecked playlists both reevaluated from the current library on
  every open/export. The editor and serialized rules no longer promise snapshots; legacy JSON and
  the legacy non-null database column remain readable without a table rebuild, and saving rules
  canonicalizes the compatibility column to the truthful always-dynamic value.
- [x] Add combined rule/limit/sort tests. Focused coverage exercises filtering before membership
  selection, item and capacity limits before presentation ordering, a randomly selected subset
  with deterministic final ordering, compound direction/tie behavior, legacy rule JSON, and
  end-to-end reevaluation of a legacy `live_updating = false` playlist.
- [x] Record implementation: date semantics in commit `93f6772`; PR #89 completes limit/sort
  ordering and removes the false snapshot option with six focused regressions.

Acceptance criteria: limiting chooses the documented subset before the independent final sort,
and the editor/persisted rule contract advertises only the always-current behavior it implements.
Existing rule JSON remains usable across the option removal.

### P2.2 Make playlist import/export transactional and deterministic

- [x] Define supported source formats and provide adapters or actionable conversion guidance
  for Apple Music XML and YouTube Music exports. Direct support is deliberately XSPF v1 only, and
  every menu, dialog, and filter now says XSPF instead of implying arbitrary playlist formats. All
  new menu, chooser, outcome, and failure text uses the existing 13-language locale catalogs.
  The namespace-aware parser requires a valid leading XML 1.0 declaration when one is present,
  `version="1"`, and the canonical XSPF namespace in default or prefixed form; validates every
  attribute's XML syntax and namespace binding; rejects DTDs, malformed/multiple/trailing
  documents, and phantom elements in
  comments, CDATA, extensions, or other nesting; and imports only direct XSPF `trackList`/`track`
  children while decoding standard named and numeric character references. Renamed Apple XML or
  arbitrary `<track>` fragments therefore fail rather than producing a misleading empty import.
  README documents exact Apple `Location`/`Name`/`Artist`/`Album`/`Total Time` and Takeout
  local-path/title/artist/album/duration mappings, links the official Apple and Google export
  instructions, and calls out catalog, missing-metadata, uploader-name, ambiguity, and
  service-transfer limitations rather than promising fuzzy cloud-to-local resolution.
- [x] Export through a sibling temporary file and atomic replacement. The complete XSPF document
  is rendered before the destination is touched, written to an exclusively created random sibling,
  flushed and `fsync`ed, then atomically persisted over the destination. XML 1.0-forbidden control
  characters are rejected before any temporary or destination file is touched. Serialization,
  write, and rename failures remove the temporary file and preserve an existing export. A corrupt
  negative stored duration or one outside Tributary's supported `u64` millisecond range is omitted
  with a warning because XSPF makes duration optional; it cannot block otherwise valid tracks from
  exporting. The GTK path runs the blocking renderer/writer with `spawn_blocking` and reports
  success and failure.
- [x] Prefer an exact existing file path before metadata matching. A valid local `file:` URI in
  `<location>` is decoded (including Windows drive-letter form) and wins when it equals a stored
  path; non-file and malformed locations are ignored as paths but may still match by metadata. The
  valid decoded source path is retained
  for the same first-priority reconciliation later. Path authority is limited to imported location
  evidence: metadata-only imports and manually added entries remain fingerprint-only even after a
  successful relink, so repeated delete/rescan cycles cannot promote a library path and let an
  unrelated track later scanned there replace the user's original choice.
- [x] Enforce the documented duration tolerance and deterministic tie-breaking. Metadata matching
  requires normalized-exact (trimmed, case-insensitive) title + artist and, when supplied, album;
  it is not fuzzy name matching. A supplied duration is a hard inclusive ±5-second gate and only
  the unique nearest candidate wins. Equal-nearest ties remain unmatched; without duration, the
  metadata match itself must be unique. Import and later orphan reconciliation share this resolver.
  Each track snapshot indexes paths and normalized metadata once instead of rescanning and
  renormalizing the full library for every playlist row. Corrupt negative library durations and
  values outside the playlist schema's non-negative `i32` range are omitted from match evidence
  instead of wrapping or blocking path/fingerprint reconciliation; already-corrupt negative
  playlist evidence is likewise treated as absent.
- [x] Return database errors rather than treating them as no-match. Matching uses one transaction's
  track snapshot instead of per-entry fallible queries hidden behind `if let Ok`; a read or write
  error now escapes as `DbErr`, reaches the UI as an explicit failure, and prevents publication.
- [x] Import playlist and entries in one transaction. Playlist creation, deterministic matching,
  preserved-entry insertion, and positions commit together; any database failure rolls everything
  back. The sidebar row is inserted only after the manager returns a committed result, and XSPF
  parsing is isolated with `spawn_blocking` before the transaction begins.
- [x] Preserve unmatched entries for later reconciliation. Rows with a valid decoded local path or
  usable title/artist fingerprint retain their original order with `track_id = NULL`, including
  optional album/duration evidence. They stay non-playable until a later scan finds one
  unambiguous local match, at which point the shared resolver relinks them. The additive nullable
  path migration is retry-safe and preserves existing entry data in both directions.
- [x] Surface matched, unmatched, and failed counts. The committed result accounts for every source
  row: uniquely linked entries are matched, retained orphans are unmatched, and rows with no usable
  identity or a valid duration too large for the database schema are failed. A syntactically
  invalid or out-of-`u64` XSPF duration is a document parse error before the transaction. The
  completion alert shows all three counts; parse, database, worker, and export failures show
  actionable alerts rather than disappearing into a silent `Option` or log-only branch.
- [x] Record implementation: PR #90. Focused coverage
  adds 27 regressions for atomic replacement and cleanup, malformed or non-XSPF input, path-first
  and normalized metadata resolution, duration boundaries and ambiguity, transactional
  rollback/database errors, retained unmatched entries, migration round trips, and outcome counts.

Acceptance criteria: a failed export cannot truncate the prior destination; a failed import cannot
publish or persist a partial playlist; each usable source row is either deterministically linked or
retained for reconciliation without guessing; every completed import accounts for matched,
unmatched, and failed rows. Blocking XML/filesystem work stays off async and GTK workers, and the UI
and README accurately advertise direct XSPF v1 support rather than Apple/Google native formats.

### P2.3 Harden tag writes

Re-scoped 2026-07-13. The temp-file-plus-rename path the review implied was missing **already
exists** in `src/local/tag_writer.rs`. The real defects are narrower and all reachable through
`write_tags` from right-click → Properties → Save in `src/ui/properties_dialog.rs`:

- [x] Validate numeric edits before rewriting the file. Year, track, and disc each used
  `else if let Ok(n) = …parse::<u32>()` with **no `else` branch**, so an unparseable value was
  silently discarded: typing `2026a` into Year rewrote the file, bumped its mtime, changed
  nothing, and reported success. `TagEdits::validate` now rejects the whole edit before any file
  is opened, the dialog surfaces it while the user can still fix it, and the numeric entries
  declare `InputPurpose::Digits`.
- [x] Attempt cleanup on every failure path. A failed `fs::rename` escaped through `?` from
  inside the success arm, orphaning `<song>.mp3.tributary-tag-tmp` next to the user's music
  forever. The temp file is now owned by an RAII guard that calls `remove_file` unless it is
  explicitly persisted, including after a failed rename. Cleanup I/O and process termination are
  inherently fallible, so the exact reserved name is also excluded from scans and watcher
  admission rather than being described as an absolute deletion guarantee.
- [x] Use an exclusively created random sibling temp path. The fixed `.tributary-tag-tmp` suffix
  created via `fs::copy` had no `O_EXCL` and no randomness, so two concurrent saves to the same
  file clobbered each other and the copy would follow a symlink planted at the predictable path.
  Temp files are now created with `create_new` (`O_EXCL`) under a random UUID name. The first real
  audio round-trip exposed a second production defect in that replacement: the randomized sibling
  ended in `.tmp`, so `lofty::read_from_path` could not determine the copied audio format and every
  otherwise-valid tag write failed before applying edits. The first extension-preserving spelling
  appended the complete source basename, which could exceed a filesystem component limit for an
  otherwise-valid long filename. The final component is bounded and source-stem-independent:
  `.tributary-tag-<canonical UUID>.<case-normalized source format extension>`.
- [x] Apply or remove the declared album-artist edit. `TagEdits.album_artist` existed and counted
  toward `is_empty()`, but `write_tags_to` never read it — the file was rewritten and the field
  ignored. It is now applied via `ItemKey::AlbumArtist`. (Still no widget sets it; implementing it
  removes the trap for whoever adds one.)
- [x] Preserve permissions and define durability/fsync behavior accurately. Unix siblings begin at
  mode `0600` before receiving the source mode on a best-effort basis. Windows snapshots the source
  DACL, creates the sibling through an exclusive no-sharing handle with `WRITE_DAC`, and installs
  that DACL before copying the first audio byte, so a more permissive parent ACL cannot briefly
  expose the copy. The tagged copy is `fsync`ed before the rename, so a crash cannot leave a
  truncated file where the original was. The module doc states these guarantees accurately.
- [x] Add focused failure, platform, and indexing tests: 10 regressions cover validation, a
  malformed number leaving the file byte-for-byte untouched with no temp behind, temp cleanup after
  a failed tag write, unsupported formats, temp-path uniqueness plus self-removal, and
  Windows-compatible flushing through a writable handle. The exclusivity test also uses a
  220-character source stem
  and uppercase extension to prove the sibling stays bounded and case-normalized. Three review
  regressions cover exact-versus-near-miss private-name recognition, initial enumeration exclusion,
  standalone temp create/remove events, and combined, tracked-split, and adjacent-split
  temp-to-original watcher renames. Concurrency is covered by the exclusivity property rather than
  by racing two threads. Exact writer-owned siblings are rejected before scan admission; watcher
  replacement queues only a metadata upsert at the public destination, never an identity-preserving
  rename from a temporary row, so slow saves retain the original track ID, history, and playlist
  links.
- [x] Add a happy-path fixture test. A committed 99-byte, 100 ms silent FLAC generated entirely
  from silence is copied into an isolated test directory and exercised through the public
  `write_tags` path. Reopening it with Lofty verifies that all ten declared fields—title, artist,
  album, album artist, genre, composer, year, track, disc, and comment—round-trip, the audio remains
  readable with its 100 ms duration, and no `.tributary-tag-*` sibling remains.
  `tests/fixtures/audio/README.md` records the generation recipe, GPL-3.0-or-later/no-third-party-
  recording provenance, and SHA-256
  `c47ed5dbe255701328f28b58fbe7408a70ae2ad20057089b5393253a00eab946`.
- [x] Record implementation: commits `6d0ec95` and `2d305e7` plus PR #91 supplied the original 11
  focused tests; PR #98 adds effective capability rehearsal, private Unix creation mode, and the
  exclusive pre-content Windows DACL path and platform regression.

Acceptance criteria: invalid edits leave the original byte-for-byte untouched, every failed save
attempts to remove its temporary sibling, and a successful public tag write preserves readable
audio while all declared fields round-trip without temporary-file residue. A cleanup failure cannot
publish the exact reserved sibling as a library row. The writer-owned filename remains bounded for
near-limit source names, and even a slow save cannot replace the original track's stable identity,
history, or playlist links.

### P2.4 Make removable-media browsing safe and asynchronous

Re-scoped 2026-07-13 and completed in two stages. PR #92 first moved the old platform-directory and
drive-letter probes to a one-shot worker. The current slice removes those heuristics entirely:
`device/usb.rs` projects cached native `gio::VolumeMonitor` metadata into plain `DeviceInfo` values
on GTK's main thread and performs no filesystem I/O. Recursive audio traversal remains a separate,
bounded background operation in `ui/source_connect.rs`, where the original symlink defect lived.

- [x] Disable symlink following during device scans. The walk used
  `WalkDir::new(mount_path).follow_links(true)`, so a USB stick containing `music -> /home/user`
  made Tributary walk the entire home directory and index it as "on the device". The traversal is
  now extracted into `enumerate_device_audio_files`, uses `.follow_links(false)`, and tests
  `entry.file_type()` rather than `Path::is_file()` — the latter follows the link anyway and would
  still have pulled in an individual file symlinked from off the device. This matches the library
  scanner's no-follow policy.
- [x] Verify descendants remain on the selected mount/device, for the symlink case: nothing outside
  the mount can now be reached through a link. (A bind mount or a nested real filesystem under the
  mount point is still followed; the library scanner's `filesystem_boundary_id` check is the model
  to copy if that matters here.)
- [x] Keep mount discovery from blocking GTK. PR #92's interim implementation isolated every
  heuristic filesystem probe on one named `usb-discovery` worker behind a capacity-one snapshot.
  The final implementation has no discovery worker or path probe to strand: `VolumeMonitor`, its
  mount objects, and their cached getters stay on the GTK main thread, while canonicalization,
  metadata probing, directory enumeration, and tag parsing never occur in monitor callbacks.
- [x] Use native platform mount/volume APIs and reconcile live hotplug/unplug updates. One
  window-owned controller takes an initial `VolumeMonitor::mounts()` snapshot, then coalesces
  mount-added and mount-changed signals onto an idle reconciliation pass. Mount-removed retires and
  removes the matching tracked path synchronously before scheduling that pass, so remove/re-add at
  the same key and path cannot be coalesced into a false no-op. Mount-pre-unmount invalidates a
  matching scan, cache, and playback before its namespace disappears, but deliberately retains the
  row and inventory until removal is confirmed because an unmount can fail. Signal closures hold
  only a weak controller; window destruction invalidates every device scan generation and
  disconnects every handler. The Devices header follows the empty/non-empty inventory, rows are
  inserted deterministically by logical key, and name changes atomically replace the row at the
  same position.

  The best available logical key is kept separate from the current native `PathBuf`. The priority
  is opaque mount UUID, volume UUID, Unix device identifier, then root URI. Shadowed and pathless
  roots are rejected, as are roots without native-path access and mounts the backend explicitly
  classifies as network or loop. A native-path mount is retained when GIO reports a removable
  drive, eject support, `device` class, or unmount support; because class metadata is optional and
  `can_unmount` is broad, this fallback can admit a non-removable or natively mounted network
  filesystem. Aliases sharing one logical key retain the lexically first path. A UUID identifies a
  logical filesystem rather than unique hardware, so clones can collide; Unix-device and root-URI
  fallbacks can change with device or path assignment.
- [x] Add malicious symlink tests: a device tree with both a directory symlink and a file symlink
  pointing off-device yields only the files physically on the device.
- [x] Add deterministic stale/duplicate-device coverage. The native policy tests reject shadowed,
  pathless, non-native, network, loop, and ineligible fixed mounts; exercise every supported
  eligibility signal and identity tier; preserve opaque identifiers; deterministically deduplicate
  aliases; and distinguish root-URI fallbacks. Pure inventory tests cover idempotence, add/rename/
  remove ordering, active removal/relocation, confirmed remove followed by same-path reattach,
  cancelled pre-unmount retention, and exact-generation reactivation that yields to later user
  navigation. There is no longer a pre-publication filesystem liveness probe: GIO removal signals
  reconcile a stale snapshot, while navigation generations prevent its retired scan from later
  caching or rendering.
- [ ] Record implementation and manual validation: symlink containment landed in commit `1886847`;
  PR #92 supplied the nonblocking one-shot bridge; PR #93 supplies the native monitor and live
  hotplug lifecycle. The final working tree passes `cargo check` and strict all-target Clippy in
  debug and release. Both profiles pass 18 library plus 557 application tests (575 each).
  Formatting, `git diff --check`, AppStream validation, and `cargo audit` also pass; the audit
  reports exactly the two already
  accepted unmaintained warnings. Twenty-six focused P2.4 tests cover traversal, native policy,
  source identity/path ownership, invalidation, bounded scanning, and lifecycle reconciliation. A
  live add, rename/change, relocation, active pre-unmount, and removal pass with real removable
  hardware is still required before this record can close. P2.5 now supplies the Flatpak
  permission/portal policy, but its installed-sandbox smoke pass also remains open.

Acceptance criteria for the implemented portion: discovery reads only cached native mount metadata
on GTK's main thread; add/change/pre-unmount/remove signals keep the sidebar inventory live without
filesystem work in callbacks; logical identity and native paths remain distinct; removal or
relocation invalidates pending navigation, track cache, and source-owned playback; an active removed
source falls back to Local, while an immediately reappearing active logical source is reselected at
its new path for a fresh scan only if the exact automatic Local fallback remains current. An
uncached device clears the prior source projection before scanning. Device audio is streamed lazily
from a named worker through a capacity-64 channel; ownership is polled every 50 ms and after every
row, closing the receiver when the generation is retired so a blocked producer wakes and stops;
its select loop uses the receive future directly and does not allocate once per discovered track.
GTK objects remain main-thread-only and symlinks are not followed. Cancellation is cooperative
rather than a hard interrupt of an in-progress kernel or tag-parser call. This is not proof of
unique physical-device identity, automount/eject/MTP support, a nested-filesystem boundary, or
sandbox-permission implementation; real-hardware validation is still outstanding.

### P2.5 Repair Flatpak behavior and local build path

- [x] Put the pinned generator where local and CI builds both use it. The byte-for-byte upstream
  script at commit `737c0085912f9f7dabf9341d4608e2a77a51a73a` is now checked in beside its
  immutable URL, MIT declaration, update procedure, and SHA-256
  `b373c8ab1a05378ec5d8ed0645c7b127bcec7d2f7a1798694fbc627d570d856c`. CI no
  longer downloads the generator script at build time; it still installs the explicitly named
  Python dependencies, whose transitive graph is not hash-locked. The deliberately deferred
  release workflow still downloads and verifies the identical generator bytes before use.
- [x] Generate `build-aux/flatpak/cargo-sources.json` consistently. One cwd-independent helper
  verifies the vendored pin, enforces Python 3.9+ and the exact direct dependency versions, reads
  the repository `Cargo.lock`, and always writes the ignored output beside the Flatpak manifest.
  CI and `scripts/build-linux.sh --flatpak` both use that helper; the Linux build helper now anchors
  itself to the repository root when invoked elsewhere. Local instructions use an isolated virtual
  environment and configure Flathub; `flatpak-builder` installs the manifest's runtime, SDK, and
  Rust extension dependencies from that remote. Flatpak-only mode bypasses host Rust/GTK checks,
  the directory source excludes known VCS/agent/generated trees (including the 36 GiB host
  `target/`), and the resulting single-file bundle records Flathub as its runtime repository. The
  PR review follow-up normalizes surrounding whitespace on otherwise exact dependency pins and
  invokes the Bash helper explicitly from CI and the Linux build script.
- [x] Define narrow USB/removable-media permissions or a portal workflow. The manifest replaces
  `home:ro` with read-only `/media`, `/run/media`, and `/mnt`, plus the documented
  `org.gtk.vfs.*` session-bus namespace. That bus grant exposes the host GVfs service methods, not
  just a single listing call; Tributary uses cached mount discovery, retains native paths only, and
  omits the GVfs filesystem sockets. It grants no raw USB/all-device, UDisks, whole-host/root, or
  whole-home library/content access; the separate theme/icon resources remain read-only. A
  fail-closed exact allowlist and negative quoted/commented/escaped-key, duplicate/missing-entry,
  and arbitrary-grant fixtures enforce all 13 reviewed finish arguments in CI. The
  automatic Devices filesystem path is read-only; Tributary still rejects non-native GIO roots.
- [x] Define writable custom-library behavior for tag editing. XDG Music remains directly
  read/write. Every other writable library must be chosen explicitly through the existing
  `GtkFileDialog::select_folder` flow; the FileChooser/Documents portals export that selected
  directory persistently with requested write permission, and Tributary stores the returned portal
  path. Host filesystem permissions remain authoritative, so the portal cannot make read-only
  storage writable.
- [x] Add an explicit identity-preserving legacy-root reauthorization flow. Preferences now offers
  a per-root **Reauthorize** action through `GtkFileDialog`, rejects non-native, non-Unicode,
  duplicate, nested, and component-overlapping endpoint scopes, and requires explicit confirmation
  that the selected folder is the same logical library. It atomically persists an immutable UUID,
  OLD, and NEW write-ahead intent while keeping OLD configured, locks the in-flight source against
  removal/supersession, and requires a restart so relocation runs before watcher installation or
  scanning. The engine consults a durable receipt before mutable config validation, scans the
  selected destination completely, requires an exact marker match for a confirmed source, and
  creates a marker for an unconfirmed markerless writable destination while deliberately leaving
  it unavailable/unconfirmed for the existing root-trust flow. A confirmed legacy identity without
  a durable marker, a markerless read-only destination, an authority change, a collision, unsafe
  path evidence, or an ambiguous scope fails closed. One guarded SQLite transaction retargets
  descendant track and imported-playlist match paths while preserving track UUIDs, metadata,
  dates, play history, and playlist foreign keys; moves root state; and writes the completion
  receipt. A retained destination authority lease is revalidated after SQL and immediately before
  commit. Receipt-backed retry resolves ambiguous COMMIT results and config-write crashes
  idempotently; inconsistent receipts, receipt-query failures, malformed request IDs without a
  matching consistent receipt, and their overlapping endpoint scopes scan neither path rather than
  risking a second identity set. Exact
  compare-and-swap config cleanup installs NEW only for the request the engine processed. Twenty-
  eight focused preference, migration, path-planning, atomicity, marker, collision, crash-recovery,
  and quarantine tests cover the contract. Implementation merged in PR #95.
- [x] Surface effective write capability in track Properties. The dialog now starts fail-closed and
  checks every exact-deduplicated selected path off the GTK thread: it must be a supported,
  readable, non-symlink regular file, and two exclusively created empty writer-namespace siblings
  rehearse flush, replace-over-existing, and explicit cleanup once per distinct parent directory,
  stopping after the first blocked directory rather than touching later parents unnecessarily. On
  Windows, every exact file separately installs its source DACL on an empty sibling and must reopen
  it for the read/write/delete rights used later, so two files in one parent cannot incorrectly
  share the first file's security evidence.
  Mode bits and path prefixes are not treated as capability because they do not describe Flatpak
  bind mounts, portal grants, ACLs, FUSE, or Windows access rules. A visible localized status in all
  13 catalogs enables fields, MusicBrainz, and Save only for a wholly capable selection, with
  automatic-device-specific Flatpak custom-library guidance on failure. Malformed or mixed
  local/remote selections no longer silently admit only a valid subset; repeated playlist paths are
  written once. Save repeats the whole check before the first write, invalidates a pending
  MusicBrainz completion, and disables Cancel/header close until the worker returns; Unix siblings
  begin at mode `0600` before a full source copy exists. Windows siblings remain exclusively held
  while the source DACL is installed before the first copied byte, closing the equivalent
  inherited-ACL exposure window. The writer remains the final fallible authority for state, space,
  cleanup, sharing, and target-specific changes after preflight.
  Sixteen focused writer, path-policy, batch, cleanup, permission/DACL, control-state, and
  messaging tests cover the slice in PR #98 (15 run on Unix and the dedicated
  exclusive-handle/DACL regression runs on Windows).
- [ ] Run a local Flatpak build and smoke-test USB/custom-library behavior. This environment has
  Flatpak 1.18 but no `flatpak-builder` or installed builder runtime, and cannot provide the
  interactive portal plus physical removable-media pass. PR #94's containerized Flatpak job proved
  the offline bundle build; a local installed-app pass must still verify XDG Music writes, a
  portal-selected custom directory across restart, a legacy direct path's reauthorization and
  fail-closed behavior, and read-only add/change/remove under each applicable standard mount root.
- [x] Record implementation: PR #94 merged with its containerized Flatpak build green. The
  noninteractive branch passed the exact vendored checksum, Python and shell syntax, YAML parsing,
  the positive/negative permission-policy suite, cwd-independent generation from `/tmp` and `/`,
  nonempty JSON parsing, and byte-identical repeated generation. No repository-root
  `cargo-sources.json` was created. PR #95 added identity-preserving legacy-root reauthorization;
  PR #98 adds effective tag-write preflight and localized Properties gating. The
  installed interactive portal/physical-media smoke task above remains open, and the release
  workflow remains intentionally out of scope. Seven of eight P2.5 tasks are now closed.

### P2.6 Synchronize packaging metadata

- [x] ~~Fix RPM `Version`, `Source0`, and `%autosetup` naming.~~ **Withdrawn 2026-07-14 — this was
  a false finding.** The in-repo `Version: v0.1.0` is a placeholder that Packit overwrites from the
  release tag at build time, so COPR ships the correct version; v0.5.0 built and released through
  this path. `build-aux/arch/PKGBUILD` is likewise rewritten at release time
  (`release.yml:500` seds `pkgver`). The checked-in literals are stale to a *reader* but never
  reach a package. Nothing to fix. Recorded rather than deleted, so the same wrong conclusion is
  not re-derived from reading the spec in isolation.
- [x] Raise GTK runtime minimum to 4.16.
- [x] Raise libadwaita runtime minimum to 1.6. Both minimums were under-declared as
  `>= 4.14` / `>= 1.5` in `build-aux/rpm/tributary.spec:23-24`, in Cargo.toml's
  `generate-rpm.requires`, and in Cargo.toml's `deb.depends`, against a crate that pins `v4_16`
  and `v1_6` (`Cargo.toml:13-14`); Arch's `PKGBUILD` declared neither floor. Debian, both RPM
  paths, and Arch now declare GTK 4.16 / libadwaita 1.6 runtime minima, and the handwritten RPM
  build requirements carry the same floors. The shipped native Linux packages therefore refuse
  to install or build on systems where the binary cannot run.
- [x] Add `%U` or `%F` to the desktop `Exec` line (`data/io.github.tributary.Tributary.desktop:6`
  was bare `Exec=tributary`). Now `Exec=tributary %U`, so the "Linux file association" feature is
  actually functional: the `MimeType` entry was present and the binary handles opens, but the
  desktop entry never passed the URI.
- [x] Add the required `AudioVideo` desktop category. `Audio`, `Music`, and `Player` are
  additional categories that the spec requires to be accompanied by the `AudioVideo` main
  category; `desktop-file-validate` now passes and runs in CI.
- [x] Add the 0.5.0 AppStream release entry and synchronize post-release development metadata.
  The AppStream history now records the shipped 0.5.0 feature release dated 2026-05-08;
  `CHANGELOG.md` archives the same release and opens an Unreleased 0.5.1 section; and Cargo package
  metadata advances to 0.5.1. Implemented on the P2.11 protected-playback branch so this inherited
  partial update is explicit and independently reviewable rather than an undocumented side change.
- [x] Update the README Rust requirement from the obsolete 1.80 floor to the actual declared
  MSRV. Commit `e6c68bc` first corrected it to 1.85; PR #100 corrects both README and Cargo to
  1.92 after proving the current locked gtk-rs graph cannot compile on 1.85.
- [x] Add CI on the declared Rust MSRV. Every toolchain in CI was
  `dtolnay/rust-toolchain@stable`; nothing verified the declared MSRV still compiled — and it did
  not: the locked graph refuses rustc 1.85 outright. The gtk-rs 0.11 release series (gtk4, glib,
  gstreamer et al.) requires rustc **1.92**, with lesser floors from `time` (1.88), `ogg_pager`
  (1.89), and the `icu_*` stack (1.86). The declared `rust-version` was a fiction from the moment
  the gtk-rs 0.11 upgrade landed. `Cargo.toml` and the README now declare 1.92 (verified locally:
  `cargo +1.85 check --locked` fails, `cargo +1.92 check --all-targets --locked` succeeds), and a
  dedicated `MSRV (1.92)` CI job compile-proves it with `--locked` on every push/PR. The job's
  toolchain pin and the `rust-version` field must move together.
- [x] Enforce the global validation gate in CI. It was a *local* gate: CI ran
  `cargo test --release` only, never `cargo test --all-targets`, so a break that
  only appears in debug (`debug_assert!`, overflow checks) shipped. Nothing ran `appstreamcli` or
  `desktop-file-validate` in any workflow, and `fuzz/` is its own workspace so neither `fmt` nor
  `clippy` ever covered it. CI now runs debug `cargo test --all-targets` and fuzz-workspace
  `fmt --check` + `clippy --locked` on Linux x86_64, and a `Desktop Metadata` job requires both
  a clean no-diagnostics desktop validation and valid AppStream metainfo on every push/PR. Eight
  repository metadata regressions keep the Rust/API floors, Debian/generated-RPM/spec-RPM/Arch
  requirements, desktop launch field, category, and exact MSRV CI pin synchronized. The fuzz
  lockfile had already drifted — its `kstring 2.0.3` requires rustc 1.96, above even the corrected
  MSRV — and is resynced to the main lock's 2.0.2; `--locked` keeps it from drifting silently again.
  Workflow-job inspection recognizes both LF and CRLF source and synthesizes a CRLF checkout in the
  MSRV regression, preventing Windows line-ending conversion from hiding a real job.
  Windows architecture jobs use `fail-fast: false`, retaining both diagnostic results if one
  runner fails. The optional setup-msys2 package cache remains enabled on x86_64 but is disabled on
  `windows-11-arm`: its action-owned `paccache` cleanup intermittently exited 127 after successful
  installation, while Cargo continues to use its separate architecture-specific cache.
- [x] Apply a redirect policy to the app's non-credential HTTP clients. Radio-Browser
  (`radio/client.rs`), IP geolocation, and MusicBrainz (`ui/properties_dialog.rs`) bypassed
  `http_security` entirely and ran reqwest defaults, so they sent a `Referer` and would follow an
  HTTPS→HTTP downgrade. They now use a shared public policy that still permits the cross-host
  mirror redirects those services depend on but refuses to be walked down to plaintext.
- [x] Record implementation: README MSRV correction in commit `e6c68bc` and non-credential
  redirect policy in commit `8368a65`; the AppStream/CHANGELOG/version synchronization landed on
  the P2.11 protected-playback branch; runtime minimums, desktop entry, the MSRV correction to
  1.92, CI gate enforcement, independent Windows matrix results, and the targeted ARM package-cache
  workaround in PR #100. P2.6 is complete.

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

- [x] Either transmit to the *selected* receiver, or remove the fallback and surface an
  actionable "install `gst-plugins-bad` for AirPlay support" error instead of silently spawning a
  subprocess that cannot work. The fallback is removed: `build_shairport_pipeline`, the `Session`
  child-process/fd plumbing, and both `cfg` variants are gone. `open_prepared_session` now gates
  every load on `ensure_raopsink`, whose failure is a localized message naming `raopsink` and
  `gst-plugins-bad`, translated in all 13 catalogs
  (`locales/*.yml` `errors.playback.airplay_raopsink_missing`). Because `PlayerEvent::Error`
  messages were previously log-only — no user would ever have seen the guidance — the window's
  error branch now also surfaces every player error as a toast; output failure messages were
  already reduced to fixed, credential- and URL-free categories, so verbatim display is safe.
- [x] Move the `which` probe (`airplay_output.rs:298`), the subprocess spawn, and teardown off the
  GTK main thread; they run synchronously under `load_uri` today. Resolved by removal: there is no
  subprocess left to probe, spawn, or tear down. The remaining teardown is a plain GStreamer
  `set_state(Null)`, identical to the local output's lifecycle.
- [x] Add a test proving a missing `raopsink` produces an actionable error rather than a silent
  no-op stream. `a_missing_raopsink_is_refused_with_install_guidance` pins the refusal at the
  policy seam (`ensure_raopsink(false)`), `a_missing_raopsink_load_fails_loudly_not_silently`
  proves the load path emits `Error` (with the actionable message) then `Stopped` — never a
  silent stream — and `raopsink_guidance_is_localized_for_every_catalog` proves every locale
  names both `raopsink` and `gst-plugins-bad` without falling back to English.
- [x] Record implementation: PR #99.

Acceptance criteria: selecting an AirPlay receiver either plays to that receiver or fails with an
error that tells the user what to install.

### P2.10 Bound MPD resolution and command ingress

- [ ] Replace blocking `ToSocketAddrs` resolution with a cancellable, deadline-bound resolver.
- [ ] Bound or deliberately coalesce the non-blocking MPD worker command queue.
- [ ] Eliminate the shared-partition race between ownership revalidation and MPD's global pause or
  stop commands, and the unguarded global side effects of load-time option resets, or require a
  detectable exclusive-control configuration.
- [ ] Retain only redacted ACK error codes so cleanup can distinguish a missing song ID from a
  permission or argument rejection without keeping server-controlled text.
- [ ] Define semantics for an external Next/queue edit that yields stopped/no current song; MPD
  exposes the same completion proof as natural queue exhaustion.
- [ ] Clean up or deliberately retain Tributary's queued ID after an observed foreign replacement
  without disturbing foreign playback.
- [ ] Add held-ACK worker FIFO, slow-first-greeting fairness, and real IPv6 loopback coverage.
- [ ] Record implementation: _pending_

### P2.11 Bound protected remote-playback startup and expose safe diagnostics

Filed 2026-07-15 from a live Windows Subsonic failure and immediately expanded when protected DAAP
playback produced the same repeated error. The initial Subsonic source connection failed after
10.001 seconds, exactly its configured connect timeout, then succeeded on retry. Protected local
playback failed after 15.004 seconds, exactly GStreamer's `souphttpsrc` blocking-I/O default; DAAP
then failed through the same shared path. Both backends converge on a dedicated `127.0.0.1` ticket,
so this evidence de-prioritizes backend authentication and catalog parsing. The code audit found
two independent defects at that boundary: `souphttpsrc` could apply an ambient proxy even to the
loopback ticket, and each ticket server discarded the previous media client's pool while waiting
without distinct upstream connect/header/body-idle deadlines or useful safe diagnostics. The
follow-up audit identified an additional shared resolver dependency: discovery discarded the
addresses received with the mDNS record, so catalogue/API access and the later protected fetch
independently re-resolved a `.local` hostname. The live Windows sequence is consistent with that
risk—the first Subsonic API attempt timed out, its retry succeeded, and both Subsonic and DAAP later
failed through the separately resolved media path—but did not prove DNS was the cause.

- [x] Reuse one credential-free upstream client per GStreamer output across per-track ticket
  servers. Bound connect establishment at 5 seconds, dispatch through response headers at 10
  seconds, and silence between body chunks at 10 seconds while imposing no total stream lifetime.
  Return deterministic empty 502/504 responses before the 30-second downstream loopback budget and
  retain exact-origin redirects, Range-only forwarding, revocation, and secret isolation.
- [x] Guarantee that validated opaque loopback tickets bypass ambient proxies for local and
  AirPlay playback. `source-setup` accepts only HTTP loopback `/cast/<UUID[.extension]>` routes with
  an explicit port and no user-info/query/fragment, installs `souphttpsrc`'s `direct://` resolver,
  disables retries, verifies the properties, and posts a fixed error plus locks the source in NULL
  if enforcement is unavailable. Ordinary files and radio retain their existing proxy policy.
  Proxy and GStreamer telemetry now records only closed phase/domain/source categories, numeric
  status/code, protected state, and bounded elapsed time; terminal watches stop after one error.
  Remote connection flows use typed authentication/connection/timeout/response categories and no
  longer log, display, or mislabel raw backend/server error strings.
- [x] Add focused accepted-without-headers, immediate-transport-failure,
  stalled/failed/active-body,
  upstream-status, exact-ticket predicate, source-signal, fixed-diagnostic, and typed remote-error
  tests. An isolated child process initializes GStreamer with both proxy environment variables
  poisoned and no bypass list, then proves the loopback fixture receives the ticket while the
  ambient proxy receives nothing. Two catalog-wide checks also prove the new user-facing error
  categories are translated without fallback and interpolate backend names in every supported
  locale. Thirteen focused additions cover this urgent slice.
- [x] Preserve addresses supplied by mDNS discovery for API and protected-media connection routing
  without changing the URL hostname, HTTP `Host`, or TLS identity. Discovery now keys exact service
  fullnames, preserves the authoritative SRV hostname, treats Avahi conflict-suffix cleanup as
  display-only, applies scheme-correct default ports, retains scoped address candidates, aggregates
  duplicate instances by exact origin, publishes address-only updates, and emits `Lost` only after
  the final retained matching instance disappears within the discovery bounds. The route is
  bounded, canonical, exact-origin, and
  ephemeral on `SourceObject`; it is never persisted or used as source identity. Each connection
  generation snapshots it immutably into the applicable API/auth client and typed stream/artwork
  requests. Current mDNS discovery supplies routes for Subsonic, Plex, and DAAP. Jellyfin's client
  accepts the same route contract, but Jellyfin UDP discovery supplies only a URL and therefore has
  no advertised-address route today. DAAP now isolates `session-id` as private query material and
  attaches a revocable session lease instead of reconstructing an authenticated URL. Protected
  playback and album art keep immutable route-keyed pools; reqwest's `resolve_to_addrs` changes only
  direct socket selection, so the hostname, HTTP authority, TLS SNI/certificate identity,
  exact-origin redirect policy, and legitimate system/explicit proxy behavior remain unchanged.
  The unchanged hostname structurally preserves TLS identity; the automated suite does not perform
  a real certificate/SNI handshake. Unauthenticated discovery state is indexed by origin and
  bounded to 512 total publications, 32 instances per exact origin, and 16 retained route
  addresses, keeping each update's aggregation work bounded. Thirty newly authored focused tests
  cover canonical and scoped routes, bounded duplicate/update/removal and cross-backend ownership,
  exact-origin mismatch, backend client/media propagation, auth-attempt route snapshots, injected
  stalled-resolver `Host`, the blocking-client path, explicit-proxy preservation, and final-loss
  invalidation before queued network work starts. The existing DAAP lifecycle regression
  additionally proves that release revokes an already-issued request, while the upgraded
  cast-proxy integration regression routes an unresolvable advertised hostname to its captured
  address and proves the exact `Host`, upstream-only authentication, receiver-header filtering,
  opaque ticket, and post-revocation 404 contract. Implemented in PR #97.
- [ ] Run full fake Subsonic and DAAP streams through GStreamer (reusing these timeout/proxy
  fixtures), confirm the packaged Windows source plugin enforces the same direct policy, and record
  live packaged-Windows playback. Support a verified alternate HTTP source or continue to fail
  closed if a package lacks `souphttpsrc`. Include strict DAAP media-header and reverse-proxy
  base-path fixtures, a Subsonic HTTP-200 error-envelope fixture, and a proxy-selected-upstream case
  so the remaining compatibility and diagnostic gaps are either fixed or explicitly bounded before
  this milestone closes.

Acceptance criteria: a protected remote load either begins playback or produces a phase-specific,
URL-free failure before the downstream source times out; a loopback ticket never visits a configured
external proxy; and logs can distinguish transport, upstream HTTP, decoder, and sink categories
without exposing credentials or filesystem paths. The urgent shared-path implementation,
deterministic proxy proof, and retained mDNS routing are complete: four of five P2.11 tasks are
closed. Packaged end-to-end validation remains open, so P2.11 is not yet closed as a milestone.

## P3 — Architecture and integration coverage

### P3.1 Introduce a source/session registry

- [ ] Define stable source IDs and backend-native track IDs. Standard backends now retain their
  native stream/artwork locators privately, but the registry and queue still use the configured
  URL string as source identity; a first-class stable source ID and its migration rules remain.
- [x] Store `Arc<dyn MediaBackend>` or a deliberate session abstraction per source. P1.6 now
  retains an `Arc<dyn RemoteMediaResolver>` behind an exact generation and random revocable lease
  for each standard remote source; the existing DAAP registry retains its stateful live backend.
- [x] Remove long-lived authenticated URLs from the generic `Track` model. Standard remote
  catalogue models retain stable app identity and backend-private locators, not stream/artwork
  requests; DAAP's generic values remain credential-free session references.
- [x] Resolve remote playable URLs/tickets at playback time. The GTK/queue value is an opaque
  exact-lease reference; consuming it yields a typed `ResolvedHttpRequest`, and the selected
  output then mints its receiver-scoped proxy ticket. Stale playback and artwork completions are
  generation- and lease-rejected.
- [ ] Resolve local/playlist media by stable track ID at playback, navigation, and receiver-load
  time so fallback reconciliation and in-flight casts cannot retain dead file paths.
- [ ] Centralize source refresh, cancellation, disconnect, and failure state. Generation/lease
  ownership and source-owned playback retirement are centralized, but environment startup,
  interactive auth, manual add, refresh publication, and UI failure handling remain separate
  paths, and DAAP still has a sibling registry because it owns an explicit logout lifecycle.
- [ ] Decide how local, radio, and external-file sources fit the same lifecycle. They deliberately
  stay on their existing direct paths in this security slice.
- [ ] Record architecture decision: _pending_
- [ ] Record implementation: P1.6 completed the remote resolver/session ownership subset in this
  PR. Stable source IDs, local/playlist resolution, unified refresh/failure state, and the
  local/radio/external lifecycle remain before P3.1 as a whole can be recorded complete.

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
- [x] `desktop-file-validate data/io.github.tributary.Tributary.desktop` with no diagnostics
- [x] `appstreamcli validate --no-net data/io.github.tributary.Tributary.metainfo.xml`
- [x] Targeted migration upgrade tests
- [x] Targeted mock-network tests
- [x] Targeted GTK/output lifecycle tests
- [x] Packaging dry runs for affected targets
- [x] Confirm `git diff --check` is clean

PR #94's containerized Flatpak build proved the manifest-local source generation and permission
policy, but a local installed interactive portal/physical-media smoke pass remains outstanding,
as does the deliberately deferred live release-workflow run.

Most recent branch validation (2026-07-16, PR #100 P2.6 packaging/CI slice): exact Rust 1.92
passes `cargo check --all-targets --locked`, while Rust 1.85 rejects the locked graph with the
documented gtk-rs 1.92 floors. Strict debug all-feature and release Clippy pass; debug and release
`cargo test --all-targets` each pass 18 library, 646 application, and 8 repository-metadata tests
(672 total per profile). The committed fuzz workspace passes format and strict Clippy with its
lockfile; `cargo fmt --all -- --check`, `git diff --check`, AppStream, and no-diagnostics desktop
validation pass; `cargo audit --no-fetch` reports only the two accepted unmaintained warnings
recorded under P0.8. The eight metadata tests cover enabled GTK/libadwaita API features, Debian
dependencies, generated and handwritten RPM runtime requirements, handwritten RPM build
requirements, Arch dependencies, exact `%U` desktop launch, the `AudioVideo` main category, and
synchronization of Cargo's MSRV with its exact CI toolchain. `rpmspec` parses both runtime/build
requirements and `makepkg --printsrcinfo` emits the versioned Arch dependencies. Temporary
`cargo-deb 3.7.0` and `cargo-generate-rpm 0.21.0` installs produced complete `.deb` and generated
`.rpm` artifacts from the release binary; the Debian control archive and RPM query both contain
the exact GTK 4.16/libadwaita 1.6 dependencies. The installed interactive Flatpak
portal/physical-media smoke task, packaged Windows/full-backend playback, and physical-media
validation remain open local/integration tasks; the release workflow remains deliberately deferred.

PR #100 CI follow-up (2026-07-16): Windows x86_64 reproduced the protected-loopback regression's
fixture failure twice while the isolated GStreamer child itself exited successfully. The original
eight-second target-listener window began before child startup and cold plugin discovery, so the
fixture could disappear before `souphttpsrc` opened. The child now owns the intended media listener
and starts its bounded window only after GStreamer initialization, while the parent keeps the
poisoned-proxy listener live until child completion and injects proxy variables through `Command`
before process creation. This preserves the end-to-end target-hit/proxy-miss assertion without
mutating a multithreaded Unix process environment. The next Windows run passed all 627 application
tests, including that regression, then exposed an unrelated LF-only workflow-job parser in the
metadata suite: a CRLF checkout made the real `msrv` job appear absent. Job extraction now operates
on logical lines independent of the newline spelling and the MSRV test synthesizes CRLF input on
every platform. The exact proxy test passes three consecutive debug and three consecutive release
runs, all 147 audio tests pass, the focused CRLF metadata regression passes, strict all-target and
release Clippy pass, and the complete release all-target/all-feature suite passes 18 library, 646
application, and 8 metadata tests (672 total). Formatting and `git diff --check` also pass. Windows
matrix jobs now retain both architecture results, and setup-msys2's optional package cache is
disabled only on ARM after its action-owned cleanup intermittently failed.

**This gate is now enforced by CI** (P2.6, closed 2026-07-16): in addition to
`cargo fmt --check`, both `clippy -D warnings` invocations, `cargo test --release`, and
`cargo audit`, CI runs debug `cargo test --all-targets`, fuzz-workspace `fmt`/`clippy --locked`,
strict no-diagnostics `desktop-file-validate`, `appstreamcli validate --no-net`, eight
repository packaging/desktop/MSRV contract tests, and an `MSRV (1.92)` compile-proof.
Checked boxes above still record the by-hand run before a milestone; the CI jobs are what catch
regressions automatically. Windows x86_64 and ARM64 are allowed to finish independently, and only
the ARM runner disables setup-msys2's optional package cache after its cleanup path intermittently
failed following a successful install; the separate Cargo cache remains enabled.

## Decisions

Record scope or design decisions here so deferred work is explicit.

- 2026-07-10 — Implemented P0.1, P0.3-P0.6, and P0.8 in PR #68. P0.7's
  workflow contract is implemented, but its live manual-dispatch acceptance test requires a
  pushed ref and remains open.
- 2026-07-15 — P2.4 uses GIO as a desktop mount inventory, not as proof of physical USB
  hardware. Cached `VolumeMonitor` objects remain on GTK's main thread; only the selected native
  path crosses to a named filesystem-scan worker. The best available key is stored separately from
  that path and prefers mount UUID, volume UUID, Unix device identifier, then root URI. This lets a
  same-key relocation retire stale navigation/cache/playback state and rescan at the new location;
  pre-unmount performs the retirement but keeps the row until removal is confirmed. UUID clones may
  collide, Unix-device and URI fallbacks can move with device/path assignment, and broad
  `can_unmount` eligibility can admit a non-removable or native-path network mount when backend
  class metadata is absent. Automount, eject, MTP/pathless browsing, hard interruption of an
  in-progress filesystem/tag-parser call, nested-mount exclusion, Flatpak access, and live
  physical-device validation were outside that implementation. P2.5 now supplies the standard
  native mount-root and custom-library portal policy; physical-device validation remains open.
- 2026-07-15 — P2.5 treats automatic Devices browsing and writable custom libraries as different
  authority paths. For library/content access, the Flatpak statically exposes XDG Music read/write
  and the conventional `/media`, `/run/media`, and `/mnt` roots read-only; separate read-only theme
  and icon grants remain UI resources. `org.gtk.vfs.*` exposes the host GVfs service namespace;
  Tributary uses its cached native mount inventory and neither requests raw USB/UDisks access nor
  exposes the GVfs filesystem sockets. A custom directory receives a persistent requested-write
  portal grant only after explicit `GtkFileDialog` selection, subject to its host permissions. A
  legacy direct root is reauthorized only through an explicit OLD→NEW write-ahead intent; ordinary
  remove-and-add remains a different operation and is not identity preserving. Startup resolves
  the intent before scanning, uses a marker-backed retained authority lease and one transactional
  row relocation, and treats its same-transaction receipt as the durable authority across an
  ambiguous commit or failed config cleanup. Confirmed sources without a supported marker and
  markerless read-only destinations remain protected rather than guessed; a newly marked legacy
  source remains unconfirmed until the normal root-trust flow completes. The permission contract
  is executable in CI. Effective-write UX now uses the writer's actual sibling-create,
  flush, replace, and cleanup mechanics on a worker rather than guessing from extensions, mode
  bits, path prefixes, or cached mount metadata. Every selected file is validated, directory
  rehearsal is deduplicated by exact parent, and the full selection is rechecked before the first
  write; this is current mechanical capability, not library-root authorization or a promise that
  later target-specific/space/mount state cannot change. Only the local installed interactive
  portal and physical-media smoke test remains required for P2.5.
- 2026-07-10 — P0.2 now fails closed for incomplete traversal, unavailable/replaced roots,
  nested mounts, mount-table failures, and ambiguous legacy databases. Legacy roots with
  existing metadata are intentionally not made deletion-authoritative from content heuristics;
  explicit trust UX and portable retained-root authority were intentionally separated from the
  safety core, and both follow-up slices were completed on 2026-07-14.
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
- 2026-07-13 — `spin 0.9.8` was replaced in `Cargo.lock` by compatible `0.9.9`, resolving the
  yanked-release warning. `cargo audit --no-fetch` now passes with exactly two unmaintained
  dependency warnings, each with an upstream disposition and a 2026-10-10-or-next-release
  review deadline under P0.8. `RUSTSEC-2023-0071` for `rsa` remains separately ignored because
  `cargo-audit` checks the inactive `sqlx-mysql` package retained in `Cargo.lock`; Tributary
  enables SQLite only, the advisory has no fixed upgrade, and the ignore must be reviewed
  immediately if MySQL support is enabled.
- 2026-07-14 — Root trust is explicit authorization, never content identification. Exact
  configured roots inherited from a pre-identity database, roots whose confirmed identity no
  longer matches, and trust requests whose complete observation has no supported audio files enter
  one FIFO main-window prompt flow. A brand-new writable root auto-enrolls only when its first
  complete observation contains supported audio and no remembered metadata. Once an empty
  observation is persisted, later content remains behind consent because it could be a removable
  or network volume newly appearing at the mountpoint. Replacement actions use destructive
  presentation, and every prompted no-supported-audio observation requires a separate second
  acknowledgement. Request correlation exposes only an opaque ID and display facts; the engine
  retains the filesystem evidence, checks the expected persisted state, creates or adopts the
  marker, freshly probes identity and mount generation, and atomically compare-and-swaps that
  expected state before accepting consent.
  Confirmation creates a random root-owned marker or adopts an already-valid marker, but the
  conversion scan cannot upsert or delete track rows. A distinct ordinary scan may run
  immediately afterward and becomes authoritative only if it is complete and still matches the
  confirmed marker/mount evidence. A deliberate decline remains fail-closed and suppresses the
  identical request for the rest of the current process; it can be reconsidered after restart or
  materially new evidence. Stale evidence, incomplete traversal, unavailable storage, and
  marker/database failure remain fail-closed, release non-active deduplication, and can be retried
  from refreshed evidence. A markerless read-only root cannot enroll, while a read-only root with
  an existing valid marker can be adopted.
  Each marker-backed root capable of authorizing mutations now acquires one retained lease over
  its exact opened root and marker for its initial scan or watcher batch on Unix and Windows.
  Authority-promoting root-state changes and mutation-bearing files, directories, and missing
  names are resolved beneath that lease without following symlink/reparse-point components or
  crossing its mount/filesystem boundary. Positive descendant handles, full retained ancestor
  chains, and retained-parent absence proofs detect root, marker, parent, and final-object
  substitution. The lease and applicable descendant evidence are revalidated after SQL changes
  inside the transaction and immediately before commit; rejection rolls back and publishes no
  success event. Fail-closed authority revocations do not require a lease.
  Filesystem-touching lease and descendant probes reached from async orchestration run on Tokio's
  blocking pool, including the final in-transaction guard. The original retained handles remain
  live in the async frame through commit or rollback. A blocking-task join failure rejects the
  current work; watcher-side failures also schedule reconciliation. The task failure is not itself
  evidence that justifies persistently demoting a root. Windows handles intentionally omit delete
  sharing so the retained namespace cannot be renamed or unlinked through commit; external
  rename/delete attempts can receive a sharing violation until the relevant scan, batch, or
  transaction releases them.
  This is an explicit filesystem/SQLite linearization boundary, not an atomic transaction shared
  by both systems: authorization linearizes at the final successful in-transaction validation,
  positive handles remain live through commit, and an absence observation is bracketed by its
  retained parent. A later filesystem change is a subsequent transition handled by watcher or
  reconciliation. The guarantee begins when the lease is acquired. A clone that already contains
  the same marker is therefore the same logical library/bearer identity for backup-and-restore
  purposes, not proof of a unique physical device; simultaneously configured duplicates still
  fail closed. The model protects ordinary local, removable, and network filesystem edits,
  replacement, hotplug, and remount races, but is a consistency boundary rather than a sandbox
  against a malicious same-user process with equivalent filesystem or mount privileges.
  A final authority probe against a slow or hung network filesystem can keep the SQLite writer
  transaction open while it waits on the blocking pool, although it no longer stalls a Tokio
  worker thread.
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
  Chromecast and MPD receive opaque app-owned proxy tickets rather than the authenticated
  upstream. Local and AirPlay playback now apply the same policy before pipeline construction:
  each protected load gets a dedicated loopback-only server and ticket, and only Tributary's
  exact-origin/no-`Referer` client sees the upstream URL. The proxy forwards only `Range`; direct
  media stays byte-for-byte direct. Malformed or unsupported protected inputs and missing runtime,
  bind, client, or ticket state fail closed. Replacement, Stop, EOS/error, setup/preroll/start
  failure, and teardown revoke the ticket, while pause/play/seek retain it only within its hard
  24-hour lifetime; identity-checked cleanup and per-load servers prevent stale callbacks from
  revoking a newer load. Server startup and route revocation run outside the proxy-state mutex;
  an allocation-identity generation lets a newer load, Stop, or runtime replacement supersede an
  in-flight startup without waiting, and prevents that older startup from installing afterward.
  This completed P1.4 without changing the generic credential-bearing `Track` representation at
  that milestone; P1.6 has since removed authenticated URLs from standard remote catalogue/UI
  values.
- 2026-07-12 — P1.5 treats every finite HTTP response as an observed byte stream rather than
  trusting `Content-Length`: Subsonic, Jellyfin, Plex, DAAP, authentication, radio/geolocation,
  artwork, and MusicBrainz reads now stop at endpoint-specific caps and carry end-to-end request
  deadlines in addition to idle-read timeouts. Async and blocking collectors classify request
  timeouts consistently, discard credential-bearing request URLs from retained body errors, and
  fail cleanly on impossible or unavailable allocations. Audio and live-radio media streams remain
  deliberately uncapped because they are length-unbounded playback transports. The Chromecast and
  MPD credential boundaries are handled by P1.6's revocable proxy tickets, and local/AirPlay fetch
  ownership is now handled by P1.4's loopback-only tickets. P1.6 subsequently made standard remote
  catalogue/UI values credential-free. Credential-bearing upstream tickets also carry P1.6's hard
  absolute lifetime rather than relying on revocation alone.
- 2026-07-14 — P1.6's receiver-ticket slice classifies every legacy/direct media URI before it can
  reach MPD. Supported credential shapes (URL userinfo; Plex `X-Plex-Token`; Jellyfin `api_key`;
  DAAP `session-id`; and Subsonic `t`/`s` or shaped `p`) require a proxy; malformed declared
  HTTP(S), credentialed unsupported schemes, and scheme-relative or malformed credential shapes
  fail closed. Non-credential radio URLs, `file:` URLs, and MPD library paths remain direct.
  A protected load first establishes the MPD TCP connection, reads that socket's local address,
  and starts one dedicated OS-port-assigned proxy on the same local IP and address family. The
  full upstream URL remains app-private; only the worker supplies it to the in-process proxy.
  Plaintext `addid`, daemon queue state, and MPD logs receive only the bracket-correct opaque
  IPv4/IPv6 ticket. Unspecified addresses and scoped or
  link-local IPv6 fail closed because they cannot produce a portable receiver URL. Runtime,
  address, bind, registration, ticket validation, and upstream body-stream errors are reduced to
  fixed URL-free categories and never fall back to the protected URL.
  Each credential-bearing ticket fixes one upstream target, uses the no-`Referer` exact-origin
  client, and forwards only `Range`. It is a replayable single-media bearer until the earlier of
  explicit revocation or a hard 24-hour expiry, not a single-use token. The deadline is absolute
  and monotonic: GET/Range requests, pause, seek, and an explicit remote Stop retaining the owned
  song do not renew it. Any replacement load (including direct or rejected media), user Stop,
  output drop, natural end, ownership loss, operation failure, worker shutdown, or stale generation
  revokes it when requested, processed, or observed, and a stale session cannot revoke a newer
  generation. A route is live only while lookup occurs strictly before its deadline; lookup at or
  after the boundary atomically removes it and returns the same 404 as an unknown/revoked ticket.
  An unrepresentable deadline fails closed as immediately expired.
  Revocation or expiry prevents future lookups but does not cancel a response the proxy already
  admitted. Local-file routes retain their server-lifetime capability contract because they do not
  front backend credentials. This TTL bounds a missed/crashed-session cleanup while the app and
  server remain alive; process/runtime death already closes the listener. Local and AirPlay now
  exchange protected requests for loopback tickets before GStreamer sees them.
  The completed resolver slice removes authenticated stream and artwork URLs from standard remote
  `Track`/album/artist/search results and GTK rows. Subsonic, Jellyfin, and Plex retain only
  backend-private native locators behind a process-owned `Arc<dyn RemoteMediaResolver>`. Every
  connection attempt registers before network I/O; only the newest attempt may install, while a
  failed newer attempt leaves the prior active lease usable. Remote publication carries an exact
  generation and random lease and synthesizes only
  `tributary-remote://<lease>/{stream,artwork}/<track-uuid>`; same-source replacement, release,
  discovery removal, manual delete, and shutdown revoke the old lease. Resolution clones the
  resolver under the registry mutex, awaits outside it, revalidates exact ownership, and attaches
  a shared revocation lease to the typed request. Playback and artwork generation checks prevent a
  stale async result from reaching an output or persistent worker. Retiring a source clears and
  stops playback only when that source owns the queue; pending resolution is invalidated without
  disturbing another source. Pause during resolution cancels that completion and leaves Play
  retryable, while an error from a protected output forces the next Play to resolve through the
  live lease again instead of replaying a stale resolved request.
  `ResolvedHttpRequest` separates a credential-free endpoint from secret request state: Plex uses
  a sensitive `X-Plex-Token` header, Jellyfin uses a sensitive `X-Emby-Authorization` header, and
  Subsonic keeps its protocol-required `u` plus `t`/`s` or HTTPS-only `p` pairs private until the
  proxy materializes them for the exact upstream request. The type is neither debuggable nor
  serializable, rejects embedded endpoint credentials and non-allowlisted auth fields, and every
  output's typed load path fails closed rather than falling through to its clean endpoint.
  Manual, saved, environment-configured, and discovered remote-server URLs are also validated as
  credential-free base URLs before persistence, auth-dialog display, logs, discovery state/UI
  publication, or ownership. Raw Jellyfin UDP discovery bodies are never logged; malformed URLs,
  userinfo, query, and fragment state fail with one fixed input-free error. This
  completes P1.6 and also completes P3.1's remote session-retention, authenticated-URL removal,
  and playback-time remote-resolution boxes. P3.1 still needs stable source IDs, stable local and
  playlist resolution, centralized refresh/failure state, and a lifecycle decision for local,
  radio, and external files.
- 2026-07-15 — A late PR #86 review found that Plex catalogue publication did not distinguish a
  track with no `Media`/`Part.key` from a resolvable track: GTK received a non-empty opaque
  reference that failed only after selection, whereas the old direct-URL path had left that row
  disabled. The follow-up omits tracks with no non-empty part and searches all media/part entries
  for a later usable locator, binding bitrate/format to the same selected media entry and
  preserving the resolver invariant that every published Plex track has a backend-private stream
  locator.
- 2026-07-12 — P1.7 places the non-`Send` Cast device, application, media session, controls,
  heartbeat, status polling, and teardown on one FIFO worker. Epoch checks bracket every Cast
  effect and event; ownership is recorded immediately after application launch and media load so
  superseded calls remain cleanable. Failures retire the session before publishing Stopped then
  Error, clean media/application ownership with fair bounded retries, and abandon an unreachable
  application after three attempts so a replacement load can reconnect. The legacy local-file
  resolver remains synchronous; P1.6's upstream ticket proxy does not change that local-path
  lookup behavior.
  `rust_cast 0.21` uses blocking `TcpStream` calls, hides the socket, and offers no timeout or
  custom-stream constructor, so cancellation can be checked only before and after an in-flight
  call; hard receiver I/O deadlines require an upstream change or audited fork and are tracked as
  P2.8 rather than overstated as part of P1.7.
- 2026-07-13 — P1.8 gives MPD one FIFO worker and one persistent TCP session per owned load.
  Stable `addid`/`playid` identity, authoritative status polling, and targeted `deleteid` cleanup
  distinguish explicit Stop and remote errors, classify stopped/no-current plus a retained owned ID
  as completion, and detect an observed replacement queue. Loads never clear the shared queue, and
  controls revalidate the current ID before acting; after ownership loss is observed, Tributary
  drops its session without mutating the foreign queue. That conservative handoff also leaves its
  own queued entry untouched; safe orphan cleanup remains P2.10 work. Every owned load explicitly
  disables MPD `repeat`, `random`, `single`, and `consume` modes so queue exhaustion remains
  attributable to Tributary. Protocol lines, responses, resolved-address counts, media-URI sizes,
  idle I/O, and post-resolution operations are bounded; poisoned streams are dropped rather than
  reused, and all diagnostics discard server text and authenticated URLs.
  Standard-library DNS resolution itself remains blocking and the nonblocking command channel is
  unbounded. MPD has no ID-scoped pause or conditional compare-and-act, so another client can still
  race between status revalidation and a global pause or stop; the load-time option resets are
  global and unguarded. MPD also exposes the same stopped/no-current proof for natural exhaustion
  and some external queue changes, while opaque synchronized ACKs cannot yet distinguish a missing
  ID from other rejections. Those narrower resilience and shared-partition improvements, plus
  deeper OS loopback coverage, are tracked as P2.10 rather than overstated as part of P1.8.
- 2026-07-15 — P1.9 separates the source whose rows remain visible from the user's current
  navigation intent. Each selection advances one monotonic generation and records an exact
  `{source_key, generation}` request; completion has three explicit dispositions: ignore a
  superseded request, cache the newest result for an inactive key, or cache and render the exact
  current request. This makes same-key re-selection ordered without discarding useful inactive
  results. A pending remote login owns navigation before network work begins even though the prior
  source can remain visible; only its exact completion may auto-select the server, and stale
  success, failure, cancellation, discovery loss, sidebar rebuild, or background publication
  cannot steal a newer intent. A rejected click while that connection is pending restores the
  pending row rather than leaving sidebar selection inconsistent with the content. The exact
  latest generation for the prior visible source remains independently refreshable while the
  remote intent is deferred, so local browser/status updates do not freeze during authentication;
  an older local generation after away-and-back navigation is still rejected.
  Playlist projection freshness is a separate engine-to-UI handoff: initial scan publishes an
  ordered invalidation after playlist reconciliation, and a watcher batch does so after it commits
  a track mutation and attempts reconciliation. The UI invalidates old playlist request
  generations before clearing the cache and any active actionable rows, then reloads only if the
  exact playlist still owns navigation. A transient playlist query failure preserves the last
  valid cache/display; only a confirmed missing playlist is intentionally represented as empty.
  Eight pure navigation tests and two engine ordering/failure-path tests cover the resulting
  contract.
- 2026-07-15 — P2.1 treats limit selection and final presentation ordering as separate phases.
  Filtering first forms the candidate set. `SmartLimit.selected_by` then chooses which candidates
  fit the item, time, or size cap; only that retained subset receives the optional compound sort.
  Previously the limit's internal selection sort ran last: it chose the correct “top 25 most
  played” membership, but replaced a requested artist/album/track presentation order with play
  count order. With no compound order, the selection order remains the visible order; a random
  limit chooses a random subset before a deterministic requested presentation sort.
  The `live_updating` checkbox is removed rather than inventing snapshot semantics around a field
  the evaluator never read. Unchecked playlists had always reevaluated dynamically, so removal
  changes no stored playlist behavior and stops promising a feature that did not exist. Serde
  continues accepting the now-unknown field in legacy rule JSON, the historical non-null SQLite
  column remains for schema compatibility, and each subsequent rule save normalizes it to true.
  A real snapshot mode would require transactional materialization, explicit initialized-empty
  state, upgrade/backfill and live/snapshot transition rules, and reconciliation semantics; it can
  be designed as a future feature rather than inferred from this legacy no-op bit. Five rule-engine
  regressions plus one database-backed compatibility/reevaluation test cover the decision.
- 2026-07-15 — P2.2 treats an imported playlist row as durable intent, not a request to guess the
  closest current song. Direct import/export is XSPF v1 only. A namespace-aware parser accepts the
  canonical namespace in default or prefixed form, validates any XML declaration plus each
  attribute's syntax and namespace binding, rejects DTDs and malformed document structure, and
  considers only direct XSPF
  track-list children. Resolution first accepts an exact local path decoded from a valid imported
  `file:` URI, then normalized-exact title/artist plus optional album; manual additions deliberately
  retain no authoritative path so a different song cannot take over an orphan merely by reusing
  its former filename. A
  supplied duration narrows candidates to the inclusive five-second window and selects only a
  unique nearest result.
  Any tie stays orphaned. This same resolver and its per-snapshot path/metadata index are reused
  after library reconciliation so an initially missing file can link later without changing the
  contract or making each entry rescan the complete library. The import reads one track snapshot
  and writes the playlist and all valid matched/orphan entries in one transaction; SQL
  errors abort rather than masquerading as a no-match. Non-file or malformed locations never become
  stored paths. Rows with neither a path nor title/artist, or a valid duration the schema cannot
  represent, are explicitly counted as failed instead of being silently dropped; invalid XSPF
  duration syntax rejects the document before the transaction. The UI exposes matched/unmatched/
  failed counts and only publishes the playlist after commit. Apple property-list XML and Google
  Takeout data remain conversion inputs rather than partially supported formats; README provides
  field-level, official-source guidance and warns where either source lacks the local identity
  needed for safe matching.
- 2026-07-15 — P2.11 treats repeated DAAP and Subsonic failures at exactly GStreamer's 15-second
  HTTP-source timeout as a shared protected-playback defect, not a protocol-authentication defect.
  The opaque loopback boundary remains necessary: handing backend requests directly to GStreamer
  would reintroduce credential exposure. Instead, only exact app-issued loopback ticket shapes
  receive a per-source direct resolver; normal internet media retains the user's proxy policy, and
  the protected upstream fetch may still use a legitimate configured proxy. `souphttpsrc` needs
  `direct://`, not an empty proxy property: under libsoup3 an empty property restores the default
  system resolver. Connect, header, and per-body-read idle budgets are separate so startup and
  wedges terminate before the downstream while healthy media has no total lifetime. The retained
  mDNS follow-up represents advertised addresses as a bounded, ephemeral capability for one exact
  scheme/hostname/effective-port origin. A connection generation snapshots that route into its
  applicable API and media clients; address updates affect the next generation, while final service
  loss clears the route and revokes current ownership. reqwest's per-host address override was
  chosen instead of globally disabling upstream proxies or rewriting URLs to an IP, preserving the
  original hostname, HTTP authority, TLS identity, and legitimate proxy policy. Jellyfin UDP
  discovery remains URL-only, and automated hostname/`Host` coverage is not a real TLS handshake.

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
| 2026-07-10 | P0.8 | PR #68 | Patched the then-failing dependencies and recorded the warnings known at the time. |
| 2026-07-13 | P0.8 follow-up | `a35cde8`, `e9a3efc` | Updated `spin` to 0.9.9, leaving exactly two time-bounded unmaintained warnings; the lockfile-only ignored RSA advisory has an explicit rationale, deadline, and feature-enable trigger. |
| 2026-07-14 | P0.2 explicit root trust | `aecbce6` | Added FIFO main-window enrollment/replacement and no-supported-audio trust-request consent with private engine evidence, guarded marker create/adopt, non-destructive conversion, and 31 focused tests. |
| 2026-07-14 | P0.2 retained root authority | `ed0a300`, `7704db8` | Retained one exact root/marker lease for each mutation-capable marker-backed root's initial scan or watcher batch, bound descendant and absence evidence beneath it, and revalidated promotions and content mutations after SQL immediately before commit. Review follow-ups preserve Windows no-delete namespace pins, move async authority I/O to blocking workers without shortening handle lifetimes, and make task failure roll back without false root demotion while watcher failures schedule reconciliation; 26 focused tests cover substitution, boundary/traversal rejection, Windows pin lifetime, rollback, and event suppression. |
| 2026-07-12 | P1.1 | `8ec84a5` | Transactional, retry-safe track-FK rebuild with dangling-link cleanup, index preservation, and scan/watcher reconciliation. |
| 2026-07-12 | P1.2 | `93d03bf`, `b961b7c`, `17babaf`, `000d9c0` | Identity preserved across authoritative paired file and directory renames; queue and active-playlist snapshots re-resolve ID-preserving committed changes by stable track ID. |
| 2026-07-12 | P1.3 | `4eb79d0` | Watchers install before scanning; bounded nonblocking ingress replays ordinary events and routes overflow, backend loss, rescan notices, and marker changes through retrying authoritative reconciliation. |
| 2026-07-12 | P1.4a | `eb5458f` | Exact-origin/no-Referer policy and URL-free errors/logging cover every then-app-owned credential HTTP fetch; Chromecast and MPD are ticketed by P1.6. |
| 2026-07-14 | P1.4b | `2188efb`, `28e3400` | Local playbin3 and AirPlay uridecodebin now receive only dedicated opaque loopback tickets for protected media; app-owned exact-origin/no-Referer fetching, Range-only forwarding, fail-closed setup, lifecycle revocation, and stale-callback isolation complete P1.4. The review follow-up moves server startup/revocation outside the state mutex and uses generation ownership so newer loads and Stop supersede in-flight startup without waiting or stale installation; 10 focused tests cover the slice. |
| 2026-07-12 | P1.5 | `842341b` | Counted finite-response reads enforce endpoint caps and total deadlines across API, authentication, DAAP, radio, artwork, and metadata clients. |
| 2026-07-14 | P1.6 receiver tickets | `c6aa7df`, `e23efd8` | Chromecast and MPD now receive revocable single-media tickets instead of backend credentials. MPD binds a dedicated IPv4/IPv6 proxy to the successful connection route, fails closed without it, and revokes across replacement, Stop, failure, ownership loss, EOS, shutdown, and stale generations; 18 new MPD/classifier/route tests cover the slice. |
| 2026-07-14 | P1.6 upstream-ticket TTL | `8735862` | Credential-bearing upstream routes now expire at a hard, non-sliding 24-hour monotonic deadline in addition to earlier lifecycle revocation. Boundary lookup atomically removes the route and returns 404; admitted responses may finish, local-file routes remain server-lifetime, and 6 deterministic tests cover the contract. |
| 2026-07-14 | P1.6 playback-time resolver | PR #86 | Standard remote catalogue/UI values carry only stable identity and exact opaque lease references. A generation-owned registry retains backend resolvers; typed playback/artwork requests isolate Plex/Jellyfin headers and Subsonic private query material until the app-owned exact-origin proxy fetch. Lease replacement/release/shutdown revokes old references and already-issued requests; unsafe manual, saved, environment, and discovery base URLs are rejected before persistence, display, logs, discovery publication, or ownership; and 36 focused tests cover request isolation, backends, native-ID collisions, registry races, stale UI work, URL rejection, and output boundaries. |
| 2026-07-15 | P1.6 Plex locator follow-up | PR #87 | Plex tracks without a non-empty media part are omitted before opaque-reference publication, while later valid media/part entries remain playable and supply their own bitrate/format; 2 focused tests cover the late PR #86 review finding and PR #87 metadata-alignment follow-up. |
| 2026-07-12 | P1.7 | `60ee2af` | One worker owns the Cast session and serializes effects, authoritative state, cancellation, cleanup retries, and stale-event suppression. |
| 2026-07-13 | P1.10 | `1c31b52` | Foreign keys, WAL, and busy timeout are set on every pooled connection instead of inherited from an sqlx default; 2 tests fail loudly if the pragma is ever lost. |
| 2026-07-13 | P2.6 (partial) | `e6c68bc`, `8368a65` | README first moved from Rust 1.80 to the then-declared 1.85 MSRV; Radio-Browser, geolocation, and MusicBrainz refuse HTTPS→HTTP redirect downgrades and send no `Referer`. The dependency graph later proved 1.85 fictional; PR #100 completes packaging and corrects the floor to 1.92. |
| 2026-07-13 | P1.8 | `eb0b9ca`, `fbaaa7f` | One persistent FIFO MPD worker provides bounded post-resolution protocol I/O, stable song identity, shared-queue preservation, ownership preflight, explicit MPD mode reset, authoritative state/position/EOS, redaction, and poisoned-stream retirement. |
| 2026-07-15 | P1.9 | PR #88 | Exact source-key/generation navigation prevents cross-source and same-key stale rendering, caches only the newest result per source, keeps the prior visible projection fresh while remote intent is pending, preserves valid caches across transient failures, and invalidates/reloads active playlists after reconciliation; eight navigation and two engine tests cover the races and event ordering. |
| 2026-07-15 | P0.4 playback-start follow-up | PR #92 | Idle Play now releases the session read used to select `StartAt` before the arm installs its queue, preventing the live Windows DAAP `RefCell already borrowed` abort; the existing Stop-then-Play regression exercises the real `RefCell` boundary and immediate mutable replacement. |
| 2026-07-15 | P2.1 | PR #89 | Smart-playlist limits choose and truncate their subset before optional compound presentation sorting; the never-enforced snapshot toggle is removed while legacy JSON/schema remain compatible and playlists explicitly reevaluate against the current library; six focused regressions cover the contract. |
| 2026-07-15 | P2.2 | PR #90 | Atomic XSPF export, transactional and loss-preserving import, exact-path then ambiguity-safe normalized metadata matching, shared reconciliation semantics, explicit result counts/errors, and native-format conversion guidance. |
| 2026-07-15 | P2.3 | `6d0ec95`, `2d305e7`, PR #91 | Numeric validation; bounded exclusive UUID-plus-format sibling files; exact scan/watcher exclusion and temp-to-original metadata refresh that preserve track identity, history, and playlist links; RAII cleanup; permission copying and pre-rename `fsync`; album-artist handling; and 11 focused tests including a public-API round trip against a generated silent FLAC fixture. |
| 2026-07-15 | P2.4 native mount lifecycle | PR #93 | GIO main-thread mount inventory and live signals; best-available logical keys separate from native paths; synchronous confirmed-removal retirement; exact-intent relocation reactivation; bounded cancellable scans; and 26 focused tests. Physical-device validation remains open; Flatpak access follows under P2.5. |
| 2026-07-15 | P2.5 Flatpak generation and access policy | PR #94 | Vendored checksum-pinned Cargo generator shared by local builds and CI; consistent manifest-local source generation; read-only standard external-media roots; reviewed GVfs bus access; portal-selected writable custom libraries; and a fail-closed permission policy test. Later P2.5 slices closed effective-write UX and identity-preserving reauthorization; installed interactive portal/physical-media smoke testing and the deliberately deferred release workflow remain open. |
| 2026-07-15 | P2.5 legacy-root reauthorization | PR #95 | Explicit portal reselection records an immutable OLD→NEW intent; a marker-backed authority lease and guarded atomic transaction preserve track identity/history and playlist links; a same-transaction receipt makes crash/ambiguous-commit recovery idempotent; and malformed, overlapping, colliding, or inconsistent states quarantine unsafe scopes. Effective-write UX follows in PR #98; installed interactive smoke testing remains open. |
| 2026-07-15 | P2.5 effective tag-write capability | PR #98 | Properties checks every exact selected local path off GTK, independently proves each Windows file's DACL access, rehearses the writer's flushed atomic replacement once per parent, and stops at the first blocked target or directory. It exposes localized all-or-none capability state, exact-deduplicates repeated playlist paths, and rechecks before the first write. Unix siblings begin private and Windows siblings install the source DACL while exclusively held before copying content. Sixteen focused tests cover the slice across Unix and Windows; only installed interactive Flatpak/physical-media smoke validation remains open in P2.5. |
| 2026-07-15 | P2.6 0.5.0 release metadata | PR #96 | Added the missing AppStream 0.5.0 release entry, archived the shipped release in the changelog, and advanced Cargo/changelog development metadata to 0.5.1. The live release-workflow verification remains deliberately deferred. |
| 2026-07-16 | P2.6 packaging and CI completion | PR #100 | Debian, generated RPM, handwritten RPM, and Arch metadata now match the GTK 4.16/libadwaita 1.6 API floor; Linux desktop activation passes `%U` and declares `AudioVideo`; Cargo/README declare the proven Rust 1.92 floor; exact-MSRV, debug all-target, fuzz, desktop, and AppStream gates run in CI; and eight repository contract tests prevent the declarations from drifting independently, including under a synthesized CRLF workflow checkout. Windows matrix jobs retain both architecture results, the ARM runner bypasses setup-msys2's intermittently failing optional package-cache cleanup while retaining Cargo caching, and the poisoned-proxy regression keeps the parent proxy fixture alive through child completion while starting the child media listener's bounded window only after GStreamer initialization, so cold Windows plugin discovery cannot create a false negative. |
| 2026-07-16 | P2.9 | PR #99 | Removed the inoperative `shairport-sync` receiver fallback, moved the remaining capability refusal ahead of media preparation, localized actionable `raopsink`/`gst-plugins-bad` guidance, surfaced safe player errors in-app, and added focused missing-element/load-path regressions. |
| 2026-07-15 | P2.11 protected-playback urgent slice | PR #96 | Shared pooled upstream transport with independent connect/header/body-idle budgets; validated direct-only local and AirPlay ticket sources; localized fixed-category, secret-free proxy/GStreamer/backend diagnostics; one-shot terminal handling; and 13 focused regressions including an isolated poisoned-proxy process plus catalog-wide translation checks. Retained mDNS routing and packaged full-backend Windows playback remain open. |
| 2026-07-15 | P2.11 retained mDNS address routing | PR #97 | Exact service-instance ownership, bounded origin-indexed duplicate aggregation, bounded ephemeral exact-origin routes through applicable API/auth clients and protected stream/artwork pools, unchanged hostname/Host/TLS/proxy behavior, pre-network loss invalidation, and DAAP bearer isolation in revocable typed requests. Thirty new focused regressions plus strengthened DAAP-lifecycle and cast-proxy integration coverage exercise route canonicalization, IPv6 scope, discovery update/removal/alias/cap semantics, stalled resolvers, explicit-proxy preservation, backend propagation, auth-attempt ownership, end-to-end Host/auth/ticket containment, and ephemeral UI identity. Full packaged-Windows/backend playback validation remains open. |
