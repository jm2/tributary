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

Progress snapshot (2026-07-18), recounted from the literal P0–P3 task checkboxes to correct the
earlier numerator/denominator drift. The live protected-playback finding recorded under P2.11 now
has eight independently verifiable tasks rather than the original three compound boxes. The
in-scope counts exclude the two deferred P0.7
live-workflow verification boxes and the withdrawn P2.6 false finding; section-summary and
global-validation gate boxes are not task progress:
**219/223 (98.2%)** in-scope checklist items complete: **50/50 P0**, **64/64 P1**, **76/79 P2**,
and **29/30 P3** after those exclusions. This incorporates the four P2.9 boxes closed by PR #99
and the seven remaining P2.6 boxes closed by PR #100, plus the five P2.7 platform-cache boxes
closed by PR #101, the four P2.8 Chromecast-deadline boxes closed by PR #102, and the three P2.10
ACK/terminal/orphan-semantics boxes implemented in PR #104, the bounded-ingress box implemented in
PR #105, the cancellable resolver box implemented in PR #106, and the
held-ACK/slow-greeting/real-IPv6 coverage box completed by PR #107, and the final exclusive-control
contract plus implementation record accepted in PR #112, since the earlier snapshot. The
deterministic protected-HTTP compatibility box under P2.11 is also complete in PR #108. The
process-isolated real-GStreamer fake-backend box under P2.11 is complete in PR #109. The packaged
Windows plugin/source-policy/decode proof is complete in PR #110 after successful native x86_64 and
ARM64 package executions; live Windows DAAP and Subsonic playback remains open. The P3.2 README
claim was re-audited and closed because the
document already labels its diagram as intended and names the shipping abstraction gaps exactly.
P3.5 now reports every Linux-host source area in one pinned aggregate, keeps different native
source sets as informational reports, and enforces the baseline accepted by two exact pinned PR
executions in PR #111.

PR #123's P3.1 production cutover closes the centralized refresh/cancellation/disconnect/failure
box while deliberately leaving the final implementation-record box unchecked, so the literal
arithmetic is now **219/223 (98.2%)** and P3 is **29/30**. `SourceRegistry` is the sole
lifecycle owner for Subsonic, Jellyfin, Plex, DAAP, and the built-in Radio-Browser source. GTK catalogue rows and
playback queues carry only `SourceId`, exact `TrackId`, and the non-secret publishing session epoch;
they no longer carry an authenticated resolver URL or lease key. One baseline/watch reducer now
projects connection, catalogue, sanitized failure, provenance, visibility, cancellation, and
retirement state into GTK. Exact accepted catalogues clear their pending guard before rebind and
reactivate an already-selected row once, while stale/superseded catalogues remain inactive;
selection helpers also release `RefCell` borrows before GTK can synchronously re-enter them.
Withdrawing Discovery clears the advertised route and revokes route-captured active/pending work
even when Saved or Environment keeps the logical row visible. Every spawned connect generation has
an exact settlement joined by the composite disconnect waiter, including superseded generations,
late rejected-adapter close, and a dissociated predecessor; final-claim release retires and prunes a
settlement-only disconnect without a second close. DAAP login is staged immediately after `mlid`,
before update/database/item catalogue work, so cancellation, replacement, malformed catalogue
responses, disconnect, route loss, and shutdown enter the same exactly-once bounded logout path.
Interactive Jellyfin login likewise stages its newly minted session token before ping/catalogue work
and gives registry retirement the only `Sessions/Logout` authority; a safely representable token is
also logged out best-effort if final client construction fails, while a hostile control-byte token
fails closed because it cannot safely form the exact logout header. Pre-existing Jellyfin API keys
and Plex legacy tokens remain non-owned durable credentials. Menu and Ctrl+Q quit requests now close
the active window through its `close-request` shutdown barrier rather than calling application quit
directly; only the no-window case takes the direct path. The focused lifecycle module passes all 53
tests. Locked debug and release suites each pass 20 library, 865 application, and 10
repository-metadata tests (**895 total**), with locked all-target/all-feature check, strict
warning-free Clippy, formatting, and diff checks green.
PR #123's CodeQL follow-up replaces three credential-shaped password literals with runtime-generated
fixture values, retaining the exact Jellyfin authentication/logout assertions without weakening or
suppressing hard-coded-secret analysis.
After that merge, the affected local `x86_64-pc-windows-gnullvm` packaging command reached the
singleton Soup import gate but exposed a second failure at a still-positional call.
`Invoke-BoundedPeImportBatch` declares a non-terminal `[string[]]` target parameter before its
stopwatch, deadlines, and limits; the reported behavior is consistent with version-dependent
PowerShell array binding/coercion treating a later control value as a target, which the inspector
correctly rejected as non-absolute or nonexistent. The current follow-up names all seven arguments
at both production call sites and pins the declaration plus each invocation independently.
PowerShell 7 parses the complete script and all 11 focused Windows packaging contracts pass, but
does not reproduce the affected host's behavior. The exact Windows PowerShell/MSYS2 packaging rerun
therefore remains pending. This repairs an already-checked packaging implementation and changes no checkbox:
the totals remain **219/223 (98.2%)**, **76/79 P2**, and **29/30 P3**. The locked all-target/all-feature
check, strict debug/release Clippy, formatting, and whitespace gates pass; complete locked debug and
release suites each pass 20 library, 866 application, and 10 repository-metadata tests (**896 total**).
PR #124's CI run `29633729566` subsequently passed every job, including production bundle/probe
execution on native x86_64 and ARM64 Windows runners. That proves the named calls on the CI
PowerShell hosts. The exact affected-host rerun on 2026-07-18 passed dependency discovery, the
release build, plugin synchronization, scanner replacement, and distribution-path resolution, but
still rejected the singleton Soup inspection target with the same absolute/existing-file
diagnostic. Explicit argument names therefore did **not** resolve the host-specific failure and the
earlier array-binding explanation remains only a disproved working hypothesis. The script now
needs a second repair that removes the ambiguous batch-target boundary or reports the exact failed
predicate before live packaged-server playback can be recorded. This does not change checklist
arithmetic.
Comprehensive lifecycle, playback-boundary, reducer, provenance, Jellyfin, and actual-wire DAAP
regressions cover the authenticated cutover. Removable and OS-opened external media still need
registry-owned at-use locator adapters; those keep the final P3.1 implementation record open.
The current P3.1 follow-up closes the separate local/playlist embedded-art authority boundary.
Artwork begins only after exact-ID resolution is still current for the configured roots and the
selected output accepts the load, then owns a cloned `ResolvedLocalMedia` through its background
parse. Cloning revalidates the root marker, ancestor chain, and exact file, so path replacement
cannot retarget the read and authority drift fails closed. The worker rewinds its possibly shared
cursor around each attempt; Lofty consumes only the extension hint (content-probing only an unknown
suffix) with property reads disabled. The same handle backs the explicit MP4 reread and raw `covr`
fallback, whose complete-file and image caps are 256 MiB and 32 MiB with checked atom arithmetic;
the ordinary parser applies the same image cap. Exact art generations discard stale results. All 9
focused art tests, the locked all-target/all-feature check, strict debug/release Clippy, formatting,
and the whitespace gate pass. Complete locked debug and release suites each pass 20 library, 872
application, and 10 repository-metadata tests (**902 total**). Because this is part of the still-open
compound implementation record, the totals remain **219/223 (98.2%)**, **76/79 P2**, and
**29/30 P3**.
The current P3.1 Radio-Browser follow-up generalizes that owner to `SourceRegistry` and installs one
stateless built-in source whose Top Clicked, Top Voted, and Near Me feeds are exact independently
cancellable `ViewOrigin` lanes. Accepted snapshots publish pathless tracks while a private payload
retains validated station-ID-to-public-URL contributions. Resolution chooses the greatest accepted
source-wide generation when views overlap, and final consumption rechecks that exact winner through
weak registry authority plus source and per-view leases; replacement, removal, disconnect, a newer
overlapping view, or dropping the last registry handle therefore revokes even an already-resolved
request. GTK retains inactive caches, distinguishes an accepted empty view from a failed successor,
and clears all three lanes plus playback on epoch loss/replacement. Near Me performs only translated
consent/navigation in GTK; an exact generation-owned prerequisite marker prevents unrelated
lifecycle invalidations from misclassifying the deliberate pre-construction dialog interval as
source loss, while a stale/superseded dialog cannot suppress ordinary fallback. Its adapter tolerates partial successful tiers, deduplicates by tier
precedence before one stable global distance sort, and retains no locator in rows or queues. This is
another part of the still-open compound record, so the literal totals remain **219/223 (98.2%)**,
**76/79 P2**, and **29/30 P3** until the removable and external-file adapters also land.
P3.3 is complete after combining the independently reviewed non-DAAP service fixture from
`b80e534`, the DAAP adversarial fixture from `6f6c9ac`, and this representative cross-service
behavior matrix. The matrix exercises rejected authentication, credential-safe authenticated and
public redirects, finite response deadlines, streaming response caps, Jellyfin and Plex
pagination, Subsonic/Plex/geolocation partial failures, and root/trailing-slash/escaped
reverse-proxy prefixes without claiming every Cartesian service/behavior pairing. It also fixed
four production gaps exposed by those fixtures: Subsonic failed envelopes no longer retain a
server-controlled message that could echo a secret; Radio-Browser and geolocation no longer accept
a valid JSON body from a non-success HTTP response; Jellyfin and Plex no longer create doubled API
paths beneath a trailing-slash prefix; and Plex stream/artwork requests no longer erase the
configured prefix or allow normalized dot segments to escape it.
P2.10 is complete in PR #112. Legacy MPD outputs remain unconfirmed until the exact endpoint is
re-added with the required localized checkbox; their public load boundary rejects playback before
optimistic Buffering state, epoch advancement, worker enqueue/cleanup, MPD connection, MPD
state/option commands, protected-media tickets, or queue mutation. The exact queue generation
remains retryable, while a second worker gate covers internal callers and media rejected before
dispatch. Two actionable Codex findings were fixed before the final `91536ab` review found no
further issues, and the exact-toolchain/native PR matrix accepted the result.
The P3.1 source-lifecycle ADR was accepted in PR #113, closing its two architecture-only decision
boxes. Independent review clarified deterministic overlapping-radio locator ownership and the
exact versioned saved-source migration/quarantine boundary before the complete exact-toolchain and
native/package matrix passed in runs 29605029668 and 29605032344; runtime implementation remains
open at that architecture-only milestone.
The bounded P3.2 local-aggregate slice accepted in PR #114 closed its three
identity/grouping/lookup boxes: local artist and album UUIDs are
stable, same-titled albums are disambiguated by effective album artist, and both formerly
unsupported by-ID track methods now resolve compact keys before narrowed SQL queries. At that
milestone the common local integration boundary, invalid persisted `TrackId` fallback, and final
P3.2 implementation record remained open. PR #116 subsequently closed the shared-catalogue seam
and P3.2 record; PR #120 preserves exact persisted local IDs and replaces the random fallback with
a frozen deterministic compatibility projection under P3.1. Static analysis passed in run 29607279056 and the complete exact-toolchain,
coverage, audit, native, Flatpak, and package matrix passed in run 29607280861. The review window
produced no code findings; Gemini posted only its service-sunset notice.
The first documentation-head rerun, 29608292265, then exposed a pre-existing MPD worker-enqueue
race: its release suite passed, but the debug suite observed terminal Shutdown already consumed and
the wake receiver dropped between protected deque insertion and wake publication, so an accepted
command spuriously reported `Disconnected`. PR #114 now keeps the same short-held mutex across the
nonblocking `try_send`, preserving the GTK no-wait boundary while making insertion/wake publication
one linearized operation. All 83 focused MPD tests pass in debug and release. Replacement run
29609489061 then passed the corrected Linux x86_64 suite, but both native Windows package probes
exposed a second pre-existing race: GStreamer could close an accepted media connection during
transition to NULL before the listener's old single stop flag was published, so an expected
incomplete header was misclassified as server corruption. PR #114 now publishes cancellation
before NULL, keeps both listeners observing through that transition, then separately stops and
drains queued accepts.
Only incomplete-header EOF/reset/abort is cancellation; malformed requests and every other server
failure remain fatal.
The first two-phase-probe head run, 29610563120, then stopped in x86_64 Windows strict Clippy
before native tests or packaging because the listener function's newly separated lifecycle flags
raised its parameter count from seven to eight. PR #114 now owns both flags in one shared lifecycle
state, keeping cancellation and final stop semantically distinct while restoring the bounded
function signature. Static run 29611191885 passed, and replacement matrix 29611194118 passed every
completed exact-toolchain, audit, metadata, coverage, Linux, Flatpak, macOS, ARM64 Windows,
packaging, and checksum job;
the ARM64 finished distribution also passed the production protected-playback probe. The x86_64
native suite passed 717 of 718 application tests but showed that the regression fixture's plain
client-socket drop did not deterministically establish its intended incomplete-header EOF on that
Windows runner. The fixture now explicitly half-closes its write side after cancellation, using
the same cross-platform EOF contract as the already-passing request-classification fixtures.
Static run 29612456247 passed, but matrix 29612458758 then reproduced the teardown failure despite
that explicit half-close; the malformed-request fixture also received a Windows response-drain
abort (716 of 718 application tests passed). The remaining production cause was the listener's
nonblocking mode: Winsock can retain it on an accepted socket,
letting a fragmented header return `WouldBlock` before cancellation is published. Accepted sockets
are now explicitly restored to blocking mode before their bounded read/write deadlines are
installed, and any configuration failure remains fatal. The malformed fixture also half-closes its
completed request before reading the response, making request completion deterministic without
relaxing the server assertion. Final static run 29613604485 and complete matrix 29613606936 then
passed: all 718 x86_64 Windows application tests were green, both Windows architectures completed
their finished-distribution protected-playback probes, and every exact-toolchain, audit, metadata,
coverage, Linux, Flatpak, macOS, package, and checksum sibling passed. No actionable automated
review thread remains.
The 2026-07-17 local `x86_64-pc-windows-gnullvm` follow-up exposed a build-helper-only path bug
before the accepted packaged-runtime probe could start: PowerShell found the caller-relative Soup
DLL beneath `$PWD`, but `.NET` expanded that same relative path beneath its unchanged process
working directory. This follow-up resolves the created distribution through the
PowerShell filesystem provider and retains its physical `ProviderPath` before constructing any
PE-inspection target. It covers the location split, repository paths containing spaces, and custom
FileSystem PSDrives whose provider-only names are unusable by `.NET` and external tools. This
repairs the local path into the already-accepted automated proof; it did not close or alter the
one remaining live-server box and left the accepted pre-seam count at **203/223 (91.0%)**.
PR #115 run 29615869107 passed every CI job, including both native Windows finished-bundle probes;
CodeQL run 29615866970 passed all three analyses, and no actionable review thread was posted.
This P3.2 completion slice makes complete track-catalogue publication a real object-safe backend
seam. Scanner snapshots construct `LocalBackend`, and every local, Subsonic, Jellyfin, Plex, and
DAAP publication path now invokes the same explicit `&dyn MediaBackend` adapter; backend-specific
`all_tracks` bypasses are removed. Authentication and media/session ownership remain concrete by
design and continue under P3.1. The production passwordless DAAP catalogue-error path logs out and
then invokes the paired user-error/GTK-cleanup helper, preventing the spinner and pending guard from
remaining live; its focused regression pins the helper's paired emissions without claiming to
induce the catalogue failure itself. The source-lifecycle decision's historical implementation
note and README now describe this shipped seam without overstating broader lifecycle convergence.
Together with PR #114's aggregate identity, grouping, and by-ID methods, this closes P3.2's final
two boxes and advances the literal count to **205/223 (91.9%)**.
PR #119 combines the independently reviewed P3.4 slices and closes all seven boxes. It covers the
production list-item action across recycling, immutable playback queues across sorted/filtered and
replaced projections, transactional output transfer/reselection, the established 83-test MPD and
delayed/adversarial Chromecast harnesses with a pre-allocation 1 MiB Cast frame cap, stale artwork
and source-result generations, and pointer-free context-menu/slider accessibility in all 13
catalogs. The final review also made wrong-type parked-output restoration fail before any playback
mutation and proved the active output, queue, and generation remain unchanged.
After rebasing over accepted PRs #117 and #118, the complete branch passes 20 library, 767
application, and 10 repository-metadata tests (797) in both debug and release plus strict Clippy in
both profiles. P3.4 is complete, advancing the literal total to **216/223 (96.9%)** and P3 to
**26/30** without changing P0–P2.
PR #120 closes only P3.1's stable source/backend-native identity box. Frozen typed identities and
saved-source schema migration replace URL ownership with persisted or deterministic `SourceId`
values across the UI and registries; every remote adapter preserves its exact bounded native
`TrackId`; and queues use `MediaKey` plus separate playlist/radio `ViewOrigin`. Removable rows use a
frozen lossless mount-relative ID, radio uses bounded station UUIDs, and external sessions mint
random source and track IDs. Version-1 saved remotes accept only random RFC UUIDv4 identities or
the exact UUIDv5 owner of the row's canonical `(backend, endpoint)`, so crafted input cannot claim
another endpoint, backend, built-in, or removable source. Repeated Add,
discovered-to-saved promotion, and saved-plus-environment startup coalesce one canonical owner;
promotion retains its advertised route for the immediate route-aware connection, and every
accepted reconnect publication clears transient connecting state even if that canonical owner was
already connected. Remote media references reversibly
prefix/hex-encode native IDs so valid `.` and `..` values cannot be normalized as URL path
segments. The local-ID-at-use slice is included but its compound box remains open until current
root acquisition, containment, exact file authority, and retention through output consumption
land. Central lifecycle/provenance and nonlocal at-use adapters also remain open. This advances the
literal total to **217/223 (97.3%)** and P3 to **27/30** without changing P0–P2.
PR #121 closes the remaining exact local/playlist ID-at-use and retained-output-authority box.
Each queue load re-reads only its exact SQLite track ID, chooses the most-specific currently
configured root backed by complete, available, identity-confirmed state, and acquires retained
root, marker, ancestor, and exact regular-file handles under a five-second outer budget. The
resolver rechecks both database bindings before publication, and the GTK handoff rejects an
in-flight parent-root result when configuration adds a more-specific root without probing the
filesystem on the UI thread. Local and AirPlay GStreamer, Chromecast, and MPD receive a typed
`ResolvedLocalMedia` lease and expose only an opaque app-owned ticket; explicit-offset reads keep
full and Range responses independent even when OS-cloned handles share a cursor. Replacement,
Stop, failure, natural completion, ticket drop, and output/server teardown revoke future lookup,
while an already admitted response remains bound to the retained file rather than a replacement
installed at its old path. Shared Chromecast cleanup preserves legacy explicit-file routes while
revoking credential and retained-authority routes. Playlist media keeps PR #120's local `MediaKey`
plus separate `ViewOrigin`. Embedded local artwork still crosses a path-based helper, and
centralized source refresh/failure/provenance plus radio/removable/external at-use adapters remain
open. PR #121
advances the literal total to **218/223 (97.8%)** and P3 to **28/30** without changing P0–P2.
Its 56 focused resolver/root-authority/playback/Cast/GStreamer/MPD regressions, locked all-target
compile, strict warning-free Clippy in debug and release, formatting, and diff checks pass locally.
The complete debug and release suites each pass 20 library, 804 application, and 10
repository-metadata tests (834 total), including every socket-bearing regression. Gemini's review
found that whole-row root-state equality also compared the observational `last_checked_at` value,
so an otherwise unchanged concurrent scan could spuriously reject playback. The post-acquisition
snapshot now binds only the exact root key, marker identity, confirmation, availability, and
complete-scan authority fields; its deterministic regression accepts timestamp-only drift and
rejects each authority-field change independently. This review fix does not alter checklist
arithmetic.
The authenticated-remote lifecycle production cutover now closes the central P3.1 box and advances
the literal total to **219/223 (98.2%)** with P3 at **29/30**. Subsonic, Jellyfin, Plex, and DAAP
construction, initial catalogue ownership, sanitized failure, at-use resolution, disconnect, and
shutdown all enter `SourceRegistry`; the old standard and DAAP registry paths are removed.
The baseline/watch reducer is the sole GTK projection path for exact generation, epoch, catalogue,
provenance, visibility, cancellation, and failure state. The exact accepted generation clears its
pending guard before row rebind/selection and can reactivate its already-selected row once the
catalogue is authoritative; stale/superseded generations remain inactive, and every synchronous
selection path drops `RefCell` borrows before GTK signal re-entry. Saved-plus-discovered rows demote
when the Saved claim is removed, while discovery route withdrawal clears the advertised route and
revokes route-capturing active or pending work even if Saved/Environment keeps the row visible.
Disconnect now joins the adopted close, a dissociated predecessor, and the exact settlement of every
still-running connect generation—including superseded construction and late rejected-adapter close.
The waiter reports any late close failure as a sanitized disconnect failure, and final claim release
retires/prunes a settlement-only disconnect without a duplicate close. DAAP login now stops after
`mlid`, before abortable catalogue work, and all later failure/retirement paths elect the same exact
logout owner. Interactive Jellyfin stages its minted token before ping/catalogue work and retires it
through one exact `Sessions/Logout`; if final authenticated-client construction fails after the token
has a safe header representation, its exact pre-authentication route attempts bounded cleanup.
Control-byte tokens instead fail closed without echo or unsafe transformed/raw logout, while existing
Jellyfin API keys and Plex tokens are never mistaken for owned sessions. Menu and Ctrl+Q quit
requests now close the active window and join its lifecycle shutdown barrier; direct application
quit remains only for the no-window case. The focused lifecycle suite passes all 53 tests. Locked
debug and release suites each pass 20 library, 865 application, and 10 repository-metadata tests
(**895 total**), with locked all-target/all-feature check, strict warning-free Clippy, formatting,
and diff checks green. At that cutover, the radio/removable/external at-use adapters and local
embedded-art authority kept P3.1's final implementation record open. The current retained-art
follow-up removes paths from local/playlist embedded-art parsing: only a clone of the exact
`ResolvedLocalMedia` accepted for output reaches the worker, which revalidates and owns its retained
file through bounded parsing. The Radio-Browser follow-up adds its stateless source, exact views,
private public locators, and final-consumption authority. Removable and external at-use adapters are
now the remaining work in the compound record, so its checklist arithmetic does not yet change.
The release-workflow dry run remains deliberately deferred rather than being counted as unfinished
P0 remediation.

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
| Generic catalogue | Subsonic, Jellyfin, Plex, and DAAP tracks retain stable application identity and exact bounded source-scoped `TrackId`; their registry-owned adapters keep stream/artwork locators, authentication, routes, leases, and DAAP session state private. A type-local Subsonic album/artist ID therefore cannot overwrite track art. |
| Source ownership | `SourceRegistry` owns the adapter, exact connection/catalogue generation, non-secret session epoch, random revocable `MediaLease`, immutable catalogue/failure state, and tracked retirement for all four authenticated backends. Replacement, release, discovery-route loss, manual deletion, or shutdown invalidates old media; DAAP's protocol-specific close performs one bounded logout through the same lifecycle. |
| GTK publication | A current authenticated-remote catalogue is converted to pathless `(SourceId, TrackId, session epoch)` rows. No server address, credential, native locator, authenticated URI, random lease key, or DAAP session key enters the GTK object or playback queue. |
| Playback and artwork | `ui/playback.rs` asks `SourceRegistry` to resolve the exact source/track/epoch only when the item is consumed. A stale epoch fails before adapter invocation; adapter, lease, and epoch are rechecked after await, and playback/artwork generations reject completion after Stop, Next, output replacement, or a newer replay. |
| Credential isolation | `ResolvedHttpRequest` is deliberately non-debuggable and non-serializable. Plex uses a sensitive `X-Plex-Token` header and Jellyfin a sensitive `X-Emby-Authorization` header. Subsonic protocol authentication remains private query material (`u` plus `t`/`s` or HTTPS-only `p`), and DAAP's bearer `session-id` is now private query material too; each is appended only inside the app-owned proxy immediately before the exact-origin fetch. |
| Output boundary | `AudioOutput::load_resolved` accepts the typed request. Chromecast, MPD, local GStreamer, and AirPlay exchange it for their existing opaque, receiver-reachable tickets; none can fall back to the clean endpoint or serialized credential state. |

The pathless source/track/epoch tuple and the output ticket are separate capabilities. The first is
non-secret identity plus freshness evidence useful only inside Tributary; it does not authorize a
request by itself. Exact current registry resolution produces a typed, revocable HTTP request, and
only then does the selected output mint a ticket reachable by its receiver. Chromecast keeps its
LAN IPv4 listener; MPD binds to the successful connection's local IP/address family; local
`playbin3` and AirPlay `uridecodebin` use dedicated loopback-only proxies. All ticket routes use
OS-assigned ports and unguessable UUIDs.

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
  `resolve_artwork` translate the exact bounded source-scoped `TrackId` through backend-private
  native locators only when playback/artwork is consumed. `ResolvedHttpRequest` separates the
  credential-free endpoint from Plex/Jellyfin sensitive headers, Subsonic's protocol-required
  private query pairs, and DAAP's bearer `session-id`. Only the app-owned exact-origin proxy
  materializes those fields.
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
through an output boundary. **Complete:** authenticated-remote tracks and GTK rows contain only
stable pathless identity plus a non-secret session epoch; DAAP's bearer session ID remains inside
its registry-owned adapter and enters only a typed, revocable request at consumption time; all
protected playback/artwork requests resolve through the exact current app-owned session and proxy;
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

- [x] Store GStreamer registries in a per-user cache directory. Bundled Windows and macOS
  startup now selects `dirs::cache_dir()/tributary/runtime/<platform>-<architecture>/<install
  fingerprint>/gstreamer/registry.bin` before GTK or GStreamer initializes. Each explicit
  environment override is preserved independently, including an intentionally empty value. If
  Windows cannot establish the preferred cache, it leaves `GST_REGISTRY` unset for GStreamer's
  normal user default and never falls back beside the executable.
- [x] Avoid writes inside `/Applications` and Program Files. The Player's late Windows
  executable-adjacent registry setup and the macOS launcher's bundle-local registry are removed.
  Runtime cache roots are projected through their nearest existing canonical ancestor before any
  directory creation, then canonicalized and checked again afterward; a direct or symlinked path
  into the application install fails closed. Sixteen focused pure/static tests cover platform,
  architecture, and install-path separation; false bundle shapes; explicit empty overrides;
  absence of install-directory fallback; direct and symlinked cache containment; cache-directory
  failure; malicious cache output; relocatable-helper normalization; atomic replacement; and
  build-script ordering.
- [x] Generate or patch the macOS pixbuf loader cache for the installed absolute bundle path. The
  app bundles, fixes, and signs `gdk-pixbuf-query-loaders`, enumerates the exact absolute loader
  modules after relocation, invokes the helper with that exact module directory, drains stdout
  and stderr concurrently under a 15-second deadline and fixed bounds, and accepts only nonempty
  UTF-8 output whose module/info/MIME/extension/signature record structure contains every expected
  module exactly once. Standalone quoted metadata—including an empty MIME or extension list—is not
  misclassified as a module. Exact absolute module records are retained; safe helper-relative
  records are accepted only when they resolve against the signed helper's top level to the exact
  expected loader set, then rewritten to C-escaped absolute installed paths. Traversal, malformed,
  duplicate, incomplete, absolute-outside, and unmatched-relative records fail closed. Only
  validated output atomically replaces a same-directory user-cache temporary after flush and
  `fsync`; failures preserve the previous cache. The copied Homebrew cache is removed before
  signing and is never patched in place.
- [x] Verify macOS signature integrity after first launch. The packaging script makes component,
  deep bundle signing, and `codesign --verify --deep --strict --verbose=2` fatal. It probes a
  read-only signed copy relocated beneath a path containing spaces with a fresh explicit cache
  root: real early setup must decode the bundled application PNG through GDK-Pixbuf, initialize
  GStreamer, find bundled `playbin3`, create both user caches with current absolute bundle paths,
  and create no cache in the `.app`. The launched copy and untouched packaged app then pass strict
  deep verification, with no later packaged-app mutation before optional DMG creation.
- [x] Record implementation: PR #101. `src/platform_runtime.rs`
  owns early bundle detection, override policy, cache derivation/containment, bounded pixbuf-cache
  generation, atomic replacement, and the hidden packaging-only probe; `build-macos.sh` owns the
  signed helper and post-sign acceptance sequence. All 14 required checks passed on 2026-07-16,
  including native macOS packaging, relocation/runtime-cache/signature probing and both Windows
  architectures; the Linux-available static and Rust gates cover the same portable policy seams.

### P2.8 Bound Chromecast control I/O

- [x] Adopt an upstream `rust_cast` timeout/custom-stream API or maintain a narrowly audited fork.
  No fork or dependency change was needed: `rust_cast 0.21` publicly exposes its generic
  `MessageManager<S: Read + Write>` and channel constructors even though its high-level
  `CastDevice` hides the socket. Tributary composes those APIs over its own deadline-aware
  `rustls::StreamOwned`.
- [x] Enforce connection, read, and write deadlines without moving a live non-`Send` Cast session
  between threads. Discovery now carries a deterministic numeric advertised IPv4 `SocketAddr`
  into the output, rejecting port zero, unspecified, loopback, multicast, broadcast, and IPv6-only
  endpoints instead of performing a later unbounded `.local` lookup. TCP connection attempts have
  a 5-second deadline. Every TLS/protocol operation has one absolute 8-second budget across all
  writes and reads plus a 2-second idle-I/O cap recalculated before each system call, so trickled
  bytes cannot renew the total deadline. The `Rc`-backed manager and channels remain constructed,
  used, and dropped on the existing dedicated FIFO worker. Transport, TLS, framing, decoding, and
  timeout failures discard the desynchronized session immediately—even when the operation became
  stale—while complete request-correlated protocol rejections retain synchronization for bounded
  best-effort cleanup. Receiver-controlled error text remains outside logs and player events.
- [x] Add a silent-receiver test proving a no-reply operation cannot pin Stop, replacement Load,
  or Shutdown forever. Real loopback peers that accept TCP but never complete the TLS exchange
  prove all three newer intents reach their fence or worker-exit acknowledgement within the short
  test budget. A byte-trickling peer proves one absolute operation deadline, and fake-transport
  regressions cover synchronized semantic cleanup, poisoned-session retirement, supersession
  during a poisoned call, and closed error classification. Discovery regressions cover
  deterministic numeric selection, unusable-address rejection, IPv6-only omission, and
  Lost-before-Found publication when an address changes.
- [x] Record implementation: PR #102; 12 focused deadline, silent-peer, cleanup/classification,
  supersession, and discovery regressions. Debug and release full suites each pass 18 library, 674
  application, and 8 repository-metadata tests (700 total per profile).

P3.4 follow-up closure (2026-07-17): P2.8 correctly left the peer-sized allocation outside its
deadline milestone because `rust_cast 0.21` reads the unsigned 32-bit length and immediately calls
`Vec::with_capacity(length as usize)`. The focused P3.4 slice now installs a narrowly scoped
plaintext framing guard between rustls and `rust_cast`: it withholds the complete four-byte header,
rejects an advertised payload above 1 MiB before upstream can observe the header or allocate, and
tracks an accepted payload to fail closed on truncation. The P2.8 deadline and session-retirement
behavior remains unchanged; the completed compound P3.4 harness item records the adversarial proof.

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

- [x] Replace blocking `ToSocketAddrs` resolution with a cancellable, deadline-bound resolver.
  PR #106 moves hostname work onto one process-lifetime `mpd-resolver` thread that owns a private
  GLib main context and every GIO enumerator, callback, and callback-owned resource. Resolution
  requests enter through a capacity-64 nonblocking channel, reject empty, NUL-bearing, or
  over-1-KiB hosts before GIO, and fail fast on overload. The service admits at most 16 requests per
  context tick and eight live GIO operations, then dispatches callbacks before another intake
  batch. Numeric IPv4 and raw or bracketed IPv6 bypass DNS after the same deadline/epoch check;
  enumerated addresses retain resolver order, deduplicate, stop at 32, and preserve IPv6 flowinfo
  and scope. The load or probe operation's one absolute deadline spans resolution, connection, and
  greeting. A waiting load rechecks its exact lifecycle epoch at most every 5 ms and cancels GIO
  when superseded, timed out, or disconnected; a late callback can only send into a retired result
  channel and is dispatched and dropped on the resolver thread. All rejection details remain
  opaque.
- [x] Bound or deliberately coalesce the non-blocking MPD worker command queue. PR #105 replaces
  the unbounded channel with a capacity-64 deque behind a short-held mutex and a capacity-one
  coalesced wake signal; enqueue never waits on channel capacity or network I/O. Commands remain an
  exact FIFO below the cap. A newer Load, Stop, or Shutdown epoch atomically purges older pending
  work and rejects late stale commands. Only a saturated same-epoch transient-control burst is
  reduced: adjacent seeks and playback-control transformations are first folded without crossing
  lifecycle or test barriers, then the oldest remaining transient is evicted if needed so the
  newest intent survives. Lifecycle commands advance the epoch and therefore cannot be crowded out.
  Stale wake tokens share one absolute receive deadline and cannot postpone authoritative polling;
  receiver loss clears pending work and fails the current operation closed. PR #114's CI follow-up
  keeps the deque lock through the capacity-one channel's nonblocking wake publication. This closes
  the terminal race in which the worker could consume Shutdown and drop its receiver after insertion
  but before `try_send`, causing an accepted command to report `Disconnected`; it adds no blocking
  operation or checklist credit.
- [x] Eliminate the shared-partition race between ownership revalidation and MPD's global pause or
  stop commands, and the unguarded global side effects of load-time option resets, or require a
  detectable exclusive-control configuration. Accepted in PR #112:
  `SavedOutput.exclusive_control` is persisted with a Serde default of false, so legacy endpoints
  fail closed. Add Output gives a localized partition-wide warning,
  keeps Add disabled until the user confirms no other MPD controller or Tributary instance shares
  the partition, and rechecks the confirmation before probing and saving. Re-adding an exact
  legacy host and port upgrades only that entry in place, preserving its name and siblings instead
  of appending a duplicate. The mode participates in `OutputTarget` identity, so false-to-true
  migration makes reselecting an already-active legacy endpoint reconstruct it rather than taking
  the same-host/port no-op path. The ordered worker receives a typed
  Unconfirmed/Exclusive mode. The public output boundary rejects every unconfirmed load before
  optimistic Buffering, output-epoch advancement, worker enqueue/cleanup, connection,
  partition-wide repeat/random/single/consume or playback-state commands, proxy-ticket
  registration, queue insertion, play, or status. Its synchronous acceptance result marks the
  exact UI queue generation retryable without invalidating the queued error, so later Play reloads
  and re-shows guidance rather than toggling an empty output. The worker independently repeats the
  gate for every load intent, including media already rejected as malformed, unsupported, or
  inactive before dispatch. Nine focused net-new tests cover legacy
  deserialization/approved serialization, exact upsert and sibling preservation, mode-sensitive
  output identity, boundary/worker zero MPD and proxy action for direct, protected, resolved, and
  pre-rejected loads, exact-generation retry behavior, and all-catalog
  warning/confirmation/failure localization.
- [x] Retain only redacted ACK error codes so cleanup can distinguish a missing song ID from a
  permission or argument rejection without keeping server-controlled text. The response parser
  retains only a closed typed code from MPD's numeric ACK header and discards the command-list
  index, echoed command name, and all trailing server text after validating that the single-command
  index is zero and the echo matches the outstanding command. Code 50 (`NoExist`) is the only ACK
  that proves a targeted queue ID is already absent; permission, argument, password, system,
  valid-but-unknown, and every other correlated rejection remain synchronized failures, never
  successful cleanup. Malformed or mismatched ACK-like input poisons the connection.
- [x] Define semantics for an external Next/queue edit that yields stopped/no current song; MPD
  exposes the same completion proof as natural queue exhaustion. After Tributary has observed its
  item active, stopped/no-current followed by a successful targeted `deleteid` is completion: the
  owned entry still existed and was atomically removed. This deliberately treats an external Next
  past the final item like natural exhaustion because MPD exposes no stronger distinction. A
  `NoExist` ACK instead proves only that the ID was already absent—whether because another client
  deleted/cleared it or for another external reason—so Tributary reports ownership loss and never
  emits a false end-of-track event; any other ACK is a real cleanup failure, not absence evidence.
- [x] Clean up or deliberately retain Tributary's queued ID after an observed foreign replacement
  without disturbing foreign playback. Unconfirmed/legacy outputs cannot load. In confirmed
  exclusive mode, a foreign current ID means the one-controller contract was violated; status and
  `deleteid` still cannot form one conditional operation, so the foreign client could select
  Tributary's ID between revalidation and deletion. Tributary therefore relinquishes the session
  and revokes any protected ticket, leaving a possibly playable direct-media entry—or a protected
  entry whose revoked opaque ticket will fail—rather than risking deletion of what has become
  current. No retained entry carries a backend credential. A stale response that discovers foreign
  ownership also drops its old session before a replacement Load can target the retained ID. This
  defense remains deliberately stricter than trusting the exclusivity promise.
- [x] Add held-ACK worker FIFO, slow-first-greeting fairness, and real IPv6 loopback coverage.
  PR #105 supplies the real-TCP held-ACK FIFO case: Pause's ACK is withheld while Seek, Play, and a
  fence enqueue promptly; no later protocol command is pipelined before the ACK, and the retained
  controls execute in exact order after release. PR #107 binds two IPv4 listeners and uses channels
  to hold the first accepted connection silent—without sleeps or an elapsed-time threshold—then
  proves the shared absolute deadline reserves enough of its budget to connect to and read the
  second address's greeting. A second real-socket regression binds `::1` when the host supports it,
  exercises numeric resolution through connection and greeting, and requires IPv6 client and peer
  addresses once that initial capability check succeeds. PR #106's raw/bracketed numeric IPv6 and
  GIO scope/flowinfo conversion coverage remains the lower-level complement.
- [x] Record implementation: partial slices include PR #104's ACK/terminal/orphan semantics
  with seven regressions, PR #105's bounded ingress/held-ACK FIFO with six regressions, and PR #106's
  cancellable bounded resolver with nine net-new regressions plus one expanded numeric-IPv6 case,
  followed by PR #107's two real-socket fairness/IPv6 regressions. PR #112 completes P2.10 with the
  persisted exclusive-control contract, exact in-place legacy upgrade, mode-aware output
  reconstruction, public-boundary and defense-in-depth worker gates, retryable synchronous
  refusal, foreign-ID conservative relinquishment, all-catalog guidance, and nine focused final
  regressions. Code-head CI run `29602279148` and the associated analysis/CodeQL checks passed on
  every supported native/package target; final Codex review of `91536ab` found no further issues.

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
- [x] Close the deterministic app-owned HTTP compatibility boundary before treating a packaged
  playback result as backend evidence. DAAP stream and artwork paths now append beneath the exact
  configured reverse-proxy prefix instead of clearing it; Subsonic API, stream, and artwork paths
  remove only one trailing empty segment, so both `/prefix` and `/prefix/` produce the same path
  without decoding and double-encoding an existing escaped prefix. DAAP's exact `Accept`,
  `User-Agent`, `Client-DAAP-Version`, and `Client-DAAP-Access-Index` values come from the same
  header map as its control client and cross the typed media boundary in a separate non-secret
  allowlist. The app-owned stream proxy and artwork worker apply those trusted headers before
  authentication; a receiver still contributes only `Range` and cannot override them. A real
  local HTTP-proxy fixture proves the protected upstream request still honors an explicitly
  selected proxy with its exact path, private query, trusted headers, and range, while a local
  Subsonic HTTP-200 fixture proves failed envelopes map to the existing typed authentication,
  token-fallback, and connection categories. Seven net-new regressions cover the path, encoding,
  header, artwork, envelope, and proxy contracts in PR #108.
- [x] Run fake Subsonic and DAAP media through the production protected local-player pipeline.
  One process-isolated regression constructs both production backend-shaped typed requests and
  sends them sequentially through the real `Player`, app-owned per-load loopback proxy,
  `souphttpsrc`, FLAC decoder, and `fakesink`. Each load must deliver a decoded audio buffer,
  publish Buffering, and reach generation-owned end-of-stream without a player error. The upstream
  fixture accepts normal GET, HEAD, and single/open/suffix byte-range semantics while proving
  DAAP's exact reverse-proxy path, private session query, and four trusted protocol headers, plus
  Subsonic's exact stream path and query multiset comprising public ID/version/client/format and
  private username/token/salt fields. Both cases prove that the GStreamer-facing URI remains an
  opaque loopback ticket with no private request material and that source setup selects
  `souphttpsrc`, direct routing, zero retries, and the 30-second downstream timeout. Missing
  player/source/sink/decoder support, a native hang, an unexpected request, a request mismatch, an
  error, or absent EOS fails rather than silently skipping; the parent kills a stuck child at an
  absolute deadline and requires a success sentinel so a misspelled libtest filter cannot pass
  with zero tests. This exercises the build host's plugins, not the bundled Windows artifact.
  Implemented in PR #109.
- [x] Prove the packaged Windows artifact carries and selects the required GStreamer runtime and
  enforces the protected-loopback policy before it is archived. The bundler now copies the exact
  architecture's `gst-plugin-scanner.exe`, places it beside Tributary and the root-level bundled
  DLLs, and computes a bounded, deduplicated transitive DLL closure seeded by the app, scanner, and
  every copied plugin. The closure uses the selected architecture's absolute
  `bin\llvm-readobj.exe --coff-imports` to inspect PE files without loading or executing them,
  queues each newly copied MSYS2 runtime into the next exact round, and never sweeps the full
  architecture `bin` directory. It inspects the Soup plugin in a singleton batch so the direct
  `libsoup` import must be observed, copied, and inspected before other targets run in batches of at
  most 28. Command-line length, combined redirected output, output lines, each inspector process,
  process-tree teardown, total targets, and the five-minute whole closure are independently bounded
  with fixed-size diagnostics and Windows PowerShell 5.1-compatible fallbacks. It then launches the
  bundled `tributary.exe` from
  the completed distribution tree with a fresh external cache, a System32-only `PATH`, and ambient
  GStreamer, GIO proxy-resolver, and HTTP proxy state removed. Co-location makes the scanner's normal
  Windows loader path identical in a user launch and the isolated probe rather than lending the
  probe a bundle-root `PATH`. A hidden early-startup probe pins plugin discovery and the registry to
  the bundle,
  rejects a missing scanner, non-bundle layout, inherited runtime override, nonempty/inside-install
  cache, external plugin provenance, or empty registry, and exits before GTK, configuration, or the
  database starts. Before GStreamer can fall back to in-process discovery, it also executes the
  exact bundled scanner with no arguments, null standard I/O, its documented exit status, and a
  five-second kill-and-wait deadline. It requires bundled `playbin3`, `souphttpsrc`, `fakesink`,
  `filesrc`, and the
  selected FLAC decoder; sends an embedded FLAC through a range-capable exact loopback ticket to at
  least one decoded buffer and EOS; observes the production source callback's direct resolver,
  zero retries, and 30-second timeout; proves a poisoned proxy receives no connection; and proves
  an alternate non-HTTP source is locked in NULL with the fixed fail-closed bus error. Factory
  plugin filenames must canonicalize beneath the bundled plugin directory. Unreadable, malformed,
  wrong-route/method, duplicate/invalid-range, trailing-byte, and response-write request failures
  fail the loopback server rather than being ignored. Its 30-second listener window starts only
  after cold GStreamer initialization and required-factory discovery; media, source, bus,
  connection, and teardown waits remain independently bounded. PowerShell applies the enclosing
  90-second process-tree deadline, a 1 MiB output-flood threshold with bounded diagnostic tails, an
  exact success sentinel, and exception-safe cleanup. Process-tree termination and exact argument
  passing feature-detect newer .NET APIs and retain bounded Windows PowerShell 5.1 fallbacks. Both
  native Windows CI architectures and the release bundle path invoke this same pre-archive script;
  the later installer-only step consumes that already-probed tree.
  Implemented and accepted in PR #110. CI run `29593455545` passed the identical pre-archive
  bundling/probe path on native x86_64 and ARM64, including the bundle-only factory/decoder,
  scanner, protected-loopback policy, real FLAC decode/EOS, poisoned-proxy, and alternate-source
  fail-closed checks. The intentionally deferred live release-workflow run is not required because
  CI invokes that same path on both supported architectures.
  PR #114 hardens a plausible listener teardown race without changing this
  completed checklist item. Production now publishes a narrow cancellation phase before
  `teardown_to_null` without stopping either listener. Both remain accepting throughout the NULL
  transition; afterward `finish_teardown` publishes a separate stop, counts every returned or queued
  accept until the nonblocking queue is empty, then joins and inspects. Only EOF,
  connection-aborted, or connection-reset I/O before complete media headers may cancel during that
  phase; malformed UTF-8, request line, route, method, header, range, timeout, other I/O, accept, and
  response-write failures remain fatal even when teardown overlaps. Four deterministic
  Windows-gated unit tests distinguish incomplete-header I/O from semantic drift, synchronize on an
  accepted media connection before proving both clean cancellation and fatal malformed input
  completed during teardown, and prove the poison observer counts one accepted plus one potentially
  queued connection across begin/finish. The x86_64 Windows job executes those unit tests. ARM64
  deliberately skips Cargo tests, but compiles the production code and executes its real two-phase
  path inside the packaged runtime probe. The transient ARM64 package false red in PR #112, run
  `29603467226`, job `87960898351`, motivated the hardening; same-source post-merge main run
  `29604363069`, job `87963920779`, passed the unchanged 498-target closure and packaged probe. Its
  fixed diagnostic does not prove that incomplete-header teardown caused the failed attempt. This
  adds neither a retry nor a weakened semantic-failure policy.
  A later local
  `x86_64-pc-windows-gnullvm` run reached the singleton Soup inspection and exposed a caller-path
  regression before the probe: the relative distribution existed beneath PowerShell's `$PWD`, but
  `Path.GetFullPath` used the unchanged process working directory and produced an absolute path to
  a nonexistent DLL. The follow-up canonicalizes the created distribution with `Resolve-Path` and
  retains its physical `ProviderPath` before any PE target is formed. A cross-platform contract
  test pins that ordering, while a live PowerShell regression (when PowerShell is installed)
  separates process cwd from a repository `$PWD` containing spaces and proves a custom FileSystem
  PSDrive resolves to the same absolute, existing physical tree rather than leaking a drive name
  that only PowerShell understands. This is a repair to the checked packaging implementation, not
  a new checklist closure.
  The next local run passed that path normalization and then failed inside the singleton Soup
  inspection with the same fixed PE-target diagnostic. `Invoke-BoundedPeImportBatch` declares its
  `[string[]] $Paths` parameter before a stopwatch and four integer controls, while both production
  callers supplied the entire argument list positionally. The affected host's behavior is consistent
  with version-dependent PowerShell array binding/coercion having sent a control value to the target
  validator. The follow-up explicitly binds `Inspector`, `Paths`,
  `ClosureClock`, both deadlines, the output cap, and the argument cap for the singleton and batched
  paths. A static regression validates the declaration and both calls independently; a complete
  PowerShell 7 AST parse and all 11 focused Windows platform-runtime tests pass on the development
  host. Locked check, strict Clippy, and complete debug/release suites also pass, with 896 tests per
  profile. This changes no import target, closure bound, runtime-copy policy, dependency, lockfile,
  or checklist arithmetic. The subsequent exact Windows PowerShell/MSYS2 rerun reached the same
  singleton Soup validation and failed with the same absolute/existing-target diagnostic despite
  those named arguments. That falsifies the argument-binding repair as a complete fix. A follow-up
  must remove the ambiguous array boundary (or first expose which exact invariant differs on that
  host), pass the native package matrix, and then pass this exact host rerun.
- [ ] Record live playback from a packaged Windows artifact against the reported DAAP and Subsonic
  servers, including catalogue connection, protected-media startup, audible playback, and useful
  URL-free failure diagnostics if either server cannot play. The automated package probe cannot
  establish live `.local`/mDNS routing, TLS/server compatibility, physical audio output, firewall
  or endpoint-security behavior, or whether DNS caused the original failures.

Acceptance criteria: a protected remote load either begins playback or produces a phase-specific,
URL-free failure before the downstream source times out; a loopback ticket never visits a configured
external proxy; and logs can distinguish transport, upstream HTTP, decoder, and sink categories
without exposing credentials or filesystem paths. The urgent shared-path implementation,
deterministic proxy and compatibility proofs, retained mDNS routing, and complete fake pipeline are
implemented: seven of eight P2.11 tasks are closed. The build-host pipeline regression and both
native packaged-Windows architectures prove the source/plugin/DLL/decode path deterministically.
Only live packaged-Windows playback against the reported servers remains open, so P2.11 is not yet
closed as a milestone.

## P3 — Architecture and integration coverage

### P3.1 Introduce a source/session registry

- [x] Define stable source IDs and backend-native track IDs. The frozen typed implementation and
  [source-lifecycle decision](architecture/source-lifecycle.md) now agree. The checked-in UUID
  namespace and canonical source spellings pin local, Radio-Browser, deterministic remote, and
  removable `SourceId` values; a new manual remote without a live owner persists a random ID,
  while promotion persists the discovered/environment row's existing deterministic ID through the
  strict version-1 envelope. Legacy arrays migrate atomically before publication or quarantine the
  whole file on conflicts. Local rows retain exact SQLite IDs; Subsonic, Jellyfin, Plex, and DAAP retain exact
  bounded song ID, item `Id`, `ratingKey`, and decimal `miid`; radio retains a bounded
  `stationuuid`; removable paths use a frozen lossless mount-relative native encoding; and
  external sessions receive independent random IDs. Queues use `MediaKey` plus a separate
  playlist/radio `ViewOrigin`, and hostile track values are bounded and redacted from debug output.
  Version-1 saved input accepts only random RFC UUIDv4 identities or the exact UUIDv5 owner of the row's
  canonical `(backend, endpoint)`; other UUID versions and UUIDv5 values owned by another remote,
  built-in, or removable source quarantine the complete file. Every
  manual/discovered/environment path coalesces the same endpoint under one
  persisted-or-deterministic owner. Promotion persists the discovered row's existing ID before
  changing its presentation and retains its ephemeral advertised route for the immediate
  route-aware authentication/connection attempt, so it neither requires a partial transfer of
  live navigation, cache, playback, or registry ownership nor loses discovery-only reachability.
  Runtime implementation is traceable through `79b9d0c`, `9bf87db`, `8232d90`, `432b66b`, and
  `269eb93`, with the accepted-reconnect follow-up in `4bb27a3` and final persisted-ID/route
  hardening in `208dbf4`; the combined milestone landed in PR #120.
- [x] Store `Arc<dyn MediaBackend>` or a deliberate session abstraction per source. P1.6 first
  retained standard remote resolvers behind revocable leases; the authenticated-remote production
  cutover now adopts Subsonic, Jellyfin, Plex, and DAAP adapters into the same
  `SourceRegistry` entry with one exact session epoch, media lease, catalogue snapshot, and
  registry-owned retirement path.
- [x] Remove long-lived authenticated URLs from the generic `Track` model. Subsonic, Jellyfin,
  Plex, and DAAP catalogue models retain stable application identity while their registry-owned
  adapters keep native stream/artwork locators, authentication, routes, media leases, and DAAP
  session state private.
- [x] Resolve remote playable URLs/tickets at playback time. Authenticated-remote GTK rows and
  queues are pathless: they retain `SourceId`, exact `TrackId`, and the non-secret session epoch
  that published the catalogue. Stream and artwork consumption ask `SourceRegistry` to
  resolve that exact epoch into a typed `ResolvedHttpRequest`; a stale epoch is rejected before
  adapter invocation and the registry rechecks adapter, lease, and epoch after the await. The
  selected output then mints its receiver-scoped proxy ticket.
- [x] Resolve local/playlist media by stable track ID at playback, navigation, and receiver-load
  time so fallback reconciliation and outputs cannot retain dead or unauthorized file paths. The
  ADR makes playlists local-library views and confines fallback matching to committed reconciliation.
  Local/playlist queue items are pathless and every initial, newly navigated, repeated, retried,
  or receiver-targeted load asynchronously re-reads the exact SQLite ID. Resolution selects the
  most-specific root that is both currently configured and backed by complete, available,
  identity-confirmed database state; a missing/dead/timed-out/stale row, root, or file fails
  without path, metadata, fingerprint, or alternate-track fallback. It acquires and retains the
  exact root, marker, ancestor, and regular-file handles under a five-second outer budget, then
  re-reads the exact track path and a semantic root-authority snapshot before publication. That
  snapshot binds the root key, marker identity, confirmation, availability, and complete-scan
  fields while deliberately ignoring the observational scan timestamp, so timestamp-only refresh
  cannot create a false stale result. The GTK thread rechecks that the retained
  root is still the most-specific current configured match, without filesystem I/O, before handing
  a typed `ResolvedLocalMedia` to the output; the bounded ticket worker revalidates physical
  authority before every retained-handle clone.
  Local and AirPlay GStreamer, Chromecast, and MPD consume only a handle-backed opaque HTTP ticket,
  never the database path. Its bounded explicit-offset stream keeps sequential and concurrent
  full/Range requests independent even when cloned OS handles share a cursor; replacement at the
  path cannot retarget an admitted request, while physical root/marker loss blocks later handle
  clones. Output replacement, Stop, load/error cleanup, end-of-queue completion, ticket drop, and
  output/server teardown retire the lease. Shared Chromecast cleanup preserves legacy
  explicit-file routes while revoking credential and retained-authority routes.
  Playlist rows now carry local source identity plus a separate `ViewOrigin`, preserving view
  scrolling while allowing generic local-source invalidation to retire a playlist-origin queue.
  GTK rows may still carry the current path for non-playback display/file actions. Implemented in
  PR #121 with exact-ID/configured-root, no-fallback, retained-handle replacement,
  symlink-escape, receiver-ticket/revocation, and playlist-invalidation regressions. The current
  retained-art follow-up clones the successfully accepted `ResolvedLocalMedia` for embedded-art
  display instead of handing its helper a playback-time path. Its worker revalidates root, marker,
  ancestor, and exact-file authority at clone, retains that handle throughout parsing, rewinds the
  shared cursor, and rejects stale generations. Lofty's bounded handle reader and the same-handle
  raw MP4 fallback cap returned art at 32 MiB; the raw fallback additionally caps the complete file
  at 256 MiB and uses checked atom arithmetic. The direct URI art helper remains only for removable
  and OS-opened external files until their at-use adapters land.
- [x] Centralize source refresh, cancellation, disconnect, and failure state. The production
  `SourceRegistry` is now the sole adapter/session authority for Subsonic, Jellyfin, Plex, DAAP,
  and Radio-Browser across environment startup, interactive authentication, manual Add, discovery,
  catalogue publication, at-use stream/artwork resolution, disconnect, route loss, deletion, and
  application shutdown. Each entry atomically owns its adapter, media lease, non-secret session
  epoch, exact connect/catalogue/view generations, sanitized failure, immutable snapshot, keyed
  provenance, and tracked retirement. The old standard/DAAP sibling registries and URL/lease-key
  UI ownership are gone.

  Sessionless standard constructors, protected interactive Jellyfin login, and DAAP's protected
  constructor enter the same staged lifecycle. Jellyfin synchronously stages an
  `AuthenticateByName` session token before ping/catalogue work and registry retirement sends the
  exact `Sessions/Logout`. If the final authenticated HTTP client cannot be built after a safely
  representable token was minted, the exact pre-authentication route attempts the same bounded
  best-effort cleanup before returning the original redacted construction failure. A hostile token
  containing control bytes cannot form the exact authorization header; that narrow case fails
  closed without echoing, transforming, or sending the value, so no unsafe logout is attempted.
  Pre-existing Jellyfin API keys and Plex legacy tokens are deliberately treated as non-owned
  durable credentials. DAAP server-info/login now returns immediately after parsing `mlid`; the
  registry installs its mandatory close guard before update, database discovery, items, or initial
  catalogue work. A cancellation or failure after that boundary therefore revokes media and elects
  exactly one bounded logout owner. Stale, cancelled, panicking, rejected, replaced, disconnected,
  route-lost, and shutdown results share the framework's exactly-once close path.

  Each spawned connect generation owns a settlement participant through construction. Before a
  late adapter can be rejected, ownership transfers into its close job, leaving no false zero-count
  window. Repeated disconnect callers share one composite waiter that joins the adopted adapter,
  any dissociated predecessor close, and every still-active connect settlement in generation order,
  including superseded generations. It completes only after constructors and closes settle and
  returns a late rejected-adapter failure as a sanitized disconnect failure. A final provenance
  release marks a settlement-only disconnect `Retired` and schedules lifecycle-owned pruning after
  the existing waiter, without issuing another cancel or close. Shutdown joins the same admitted
  work. Menu and Ctrl+Q quit requests close the active window so its `close-request` handler owns and
  awaits that barrier; only a no-window application quits directly. Stream and artwork resolution
  require the catalogue's exact epoch before adapter invocation and recheck the current
  adapter/lease/epoch after asynchronous resolution.

  Radio-Browser is installed lazily as one stateless built-in session; no caller-supplied factory
  or generation callback runs beneath its installation mutex. Each exact view refresh owns lifecycle
  cancellation and publishes either an accepted pathless snapshot, a closed failure category, or
  cancellation. Public locator contributions remain private and have their own accepted-view lease.
  Overlapping station IDs resolve from the greatest accepted global generation and final
  consumption rechecks that exact winning generation through weak registry authority, preventing a
  pending request from retaining the registry or surviving view/source replacement. A failed
  refresh preserves its predecessor while a successful empty feed replaces it authoritatively.

  One atomic lifecycle baseline plus a monotonic invalidation watch now drives a GTK reducer; the
  UI no longer infers lifecycle authority from row spinners, URLs, or channel closure. Exact
  generation-correlated cancellation and closed failure categories clear only their owning intent,
  same-epoch catalogue refresh preserves playback/navigation, and a replacement epoch invalidates
  stale media before publishing its successor. Catalogue acceptance clears its exact pending guard
  before any connected-row rebind or selection. If the accepted row was already selected but that
  guarded rebind did not activate the authoritative catalogue, selection is invalidated and restored
  to the exact accepted index; stale/superseded catalogues cannot trigger that plan. Programmatic
  selection and fallback helpers snapshot active navigation keys and release `RefCell` guards before
  GTK emits synchronous callbacks. Saved, Environment, and Discovery publishers own independent
  keyed/refcounted claims. Removing Saved demotes a still-discovered row instead of deleting it;
  removing Discovery clears its advertised route and revokes any route-bound active or pending
  adapter even when Saved or Environment keeps the logical row visible. The reducer owns the
  corresponding cache, playback, navigation, and presentation demotion. Hidden or absent snapshots
  authoritatively clear pending state, cache, playback, navigation, rows, and empty category headers.
  Lifecycle-owned pruning waits for retirement without a second disconnect mutation.

  Comprehensive deterministic foundation, registry-wrapper, reducer, playback-boundary,
  provenance, shutdown, Jellyfin session-policy, and actual-wire DAAP tests cover protected login
  cancellation, supersession, malformed post-login catalogue responses, exact logout, invalid
  minted-token containment, durable credential non-revocation, stale-epoch stream/artwork rejection,
  failure correlation, accepted-catalogue activation, demotion/visibility, and retirement races.
  The focused lifecycle module passes all 53 tests. Locked debug and release suites each pass 20
  library, 865 application, and 10 repository-metadata tests (**895 total**), with locked
  all-target/all-feature check, strict warning-free Clippy, formatting, and diff checks green.
- [x] Decide how local, radio, and external-file sources fit the same lifecycle. Local is one
  always-registered source, playlists are local views, Radio-Browser is one stateless source whose
  feeds are views, removable filesystems are generation-owned sources keyed by their existing
  logical GIO identity, and the first playable file in each OS-open delivery is an ephemeral
  one-item source. The local/playlist embedded-art authority boundary is now implemented;
  Radio-Browser now uses its specified registry/view adapter and at-use resolver. Removable and
  external-file adapters remain on the current direct paths. Recorded
  in the [source-lifecycle decision](architecture/source-lifecycle.md), accepted in PR #113.
- [x] Record architecture decision: [Source identity and lifecycle ownership](architecture/source-lifecycle.md).
  The document distinguishes accepted decisions, existing foundations, remaining implementation,
  migration, and completion tests. Accepted in PR #113 after the full native/package matrix passed.
- [ ] Record implementation: P1.6 completed the remote resolver/session ownership subset in PR
  #86 and PR #113 closed the architecture boxes. PR #120 completes frozen identity types,
  saved-source migration/quarantine, stable source ownership, exact native IDs,
  `MediaKey`/`ViewOrigin` queues, and radio/removable/external identity. PR #121 closes exact
  local/playlist ID-at-use plus retained root/file authority through every output. PR #122 fixes
  the central state/owner/task/provenance/shutdown API and race contracts; the following production
  cutover makes that authority the sole owner for Subsonic, Jellyfin, Plex, and DAAP and moves GTK
  to pathless epoch-bound catalogue/queue state. PR #125 moves local/playlist embedded-art parsing
  onto cloned, revalidated `ResolvedLocalMedia` authority after output acceptance. The current
  follow-up adds Radio-Browser's registry/view resolver; retained removable
  and ephemeral external-file at-use adapters must still land before P3.1 itself can be recorded
  complete.

### P3.2 Make the backend abstraction real and stable

- [x] Construct and use `LocalBackend` through the same integration boundary. Complete catalogue
  publication now has one object-safe `MediaBackend::list_tracks` operation and one explicit
  `&dyn MediaBackend` adapter. Full local scanner snapshots construct `LocalBackend`; environment,
  manual, discovery, and authenticated Subsonic, Jellyfin, Plex, and DAAP catalogue paths use the
  same adapter. Backend-specific `all_tracks` methods are removed, and a trait-object spy pins
  dynamic dispatch without conflating the still-distinct authentication and protected-media
  lifecycles. The production passwordless DAAP catalogue-error branch logs out before invoking a
  paired user-error/GTK-cleanup helper; its focused unit regression pins those paired emissions.
  PR #116.
- [x] Replace ephemeral album/artist UUIDs with stable identities. PR #114 derives local
  artist and album UUIDv5 values under one private versioned namespace with distinct domains,
  component-count and length framing, and pinned golden values. Artist identity uses the exact
  stored performing-artist name; album identity uses the grouping key below. No case folding,
  trimming, Unicode normalization, year, or location enters the UUID. Every converted local
  `Track` carries matching aggregate IDs, while `Album.artist_id` deliberately remains `None`
  because a compilation album credit need not identify a performing-artist entity.
- [x] Group local albums by a disambiguating key, not title alone. PR #114 groups by exact
  `(album_title, effective_album_artist)`. Only a missing or Unicode-whitespace-only album-artist
  tag falls back to the exact track artist; every nonblank value, including case, normalization,
  and surrounding whitespace, is preserved. Album listings, per-artist album counts, track album
  IDs, and library album statistics share that key. SQL pre-aggregates exact metadata fragments;
  the final Rust fold groups compilation performers consistently and uses deterministic numeric
  and lexical minima for year and genre instead of SQLite bare-column selection.
- [x] Implement or remove unsupported trait methods. PR #114 implements
  `get_album_tracks` and `get_artist_tracks`. Each maps the UUID through compact distinct aggregate
  keys, returns an empty vector when no key exists, then queries full track models only for the
  exact album title or performing artist under deterministic ordering. Album results retain the
  shared exact effective-artist predicate after the title-constrained SQL query, so blank-tag
  fallback and same-title disambiguation cannot diverge from listing/ID semantics.
- [x] Align README architecture claims with actual code. README explicitly labels its diagram as
  the intended architecture. It first recorded the absence of any trait-object/local seam in
  `e6c68bc`; this completion slice updates the claim to the now-shipping shared catalogue boundary
  while retaining the concrete authentication/lifecycle limitations tracked under P3.1.
- [x] Record implementation: PR #114 accepted stable aggregate identity, same-title
  disambiguation, and both formerly unsupported by-ID methods. This completion slice constructs
  `LocalBackend`, routes every complete local and remote catalogue through the shared trait-object
  adapter, removes the backend-specific `all_tracks` bypasses, and updates the public architecture
  claims. At PR #116, repairing or rejecting an invalid persisted local track UUID instead of the
  then-existing random conversion fallback remained assigned to typed `TrackId` work in P3.1 and
  did not block this bounded P3.2 completion. PR #120 subsequently preserved the exact persisted
  string and replaced that fallback with a frozen deterministic compatibility projection.

Acceptance criteria for this bounded slice: unchanged exact local metadata yields identical
artist/album identities across queries and restarts; concatenation or artist/album domain
collisions cannot alias; a compilation and same-titled albums group by the documented effective
album artist; listing IDs round-trip through both by-ID methods; unknown IDs return no tracks; and
the lookup's full-model query is constrained to the resolved title or exact artist. Complete
catalogue publication must additionally enter the shared trait-object adapter. The path-bearing
queue and invalid persisted-`TrackId` gaps recorded at P3.2 completion were subsequently addressed
under P3.1; current-root authority, containment, and retention remain there.

### P3.3 Add network integration harnesses

- [x] Mock Subsonic, Jellyfin, Plex, DAAP, Radio-Browser, and geolocation services.
  - The five non-DAAP production success paths share one loopback fixture that matches concurrent
    requests by method, exact path, and decoded query subset; queues bounded replies; records
    bounded bodies and headers; treats an unexpected, ambiguous, over-count, or missing request as
    a test failure; and performs explicit five-second graceful shutdown with abort-on-drop cleanup.
  - Subsonic's existing live regression now proves every API request carries the expected username,
    protocol/client/format values, salt, and correctly derived token without the legacy plaintext
    parameter. Jellyfin and Plex tests connect complete backends, prove credential headers on all
    five expected requests, filter non-music libraries, and map one track/album/artist catalogue
    despite Jellyfin's concurrent item fetches. Radio-Browser proves its request contract and
    rejects a returned `file:` stream; geolocation proves a valid first provider ends the cascade
    before either later endpoint is contacted.
  - DAAP uses a separate protocol-appropriate raw-socket fixture. It is deterministic,
    endpoint-scripted, deadline/header-capped, fragmentation-forcing, stateful, and owns its handler
    tasks. Integrating that fixture with the five non-DAAP success fixtures closes this all-services
    box. Independent reviews of source commits `b80e534` and `6f6c9ac` reported no actionable
    findings.
- [x] Cover auth, redirect, timeout, body cap, pagination, partial failure, and reverse-proxy
  prefix behavior.
  - This is a representative matrix, not a misleading Cartesian claim. Rejected token/password
    authentication is driven through production Subsonic, Jellyfin, and Plex entry points with
    typed errors that omit the submitted secrets even when Subsonic echoes one in its failed
    envelope; DAAP's production session fixture separately covers HTTP and in-band session
    rejection. Jellyfin follows a relative same-origin ping
    redirect while retaining its required authorization and sending no `Referer`, Radio-Browser
    follows its credential-free public redirect, and the shared policy regressions prove an
    authenticated cross-origin redirect stops before the destination receives a credential.
  - The keyed fixture can delay a response body without an unbounded peer. Radio-Browser and the
    geolocation cascade use their production request/read paths with a test policy whose short
    finite deadline and small streaming cap make late and oversized valid JSON fail quickly; the
    production policy remains 15 seconds with its 8 MiB station and 256 KiB geolocation caps.
    These regressions also exposed and fixed both paths accepting valid JSON from HTTP 5xx bodies.
  - Jellyfin consumes a full 5,000-item page plus a final page, and Plex consumes a full 1,000-item
    page plus a final page, through complete backend connections. Subsonic retains a healthy
    artist/album/track while independently failed artist and album requests are skipped; Plex
    publishes one complete healthy section while an unavailable section contributes nothing; and
    geolocation advances past HTTP-error, oversized, and timed-out providers without publishing
    their data. The production catalogue fixtures assert the exact surviving native IDs:
    Subsonic `healthy-track`, Jellyfin and Plex `track-0`, and DAAP decimal `9`.
  - Complete backend fixtures use configured prefixes ending in `/` and prove API, pagination,
    stream, and artwork paths. Existing Subsonic/DAAP exact-path regressions cover root, ordinary,
    trailing-slash, and already-escaped bases. New Jellyfin/Plex regressions cover the same four
    base shapes, while Plex also pins exact one-empty-segment behavior for root `//` and prefixed
    `/share//` bases. Their API builders now remove only one trailing empty base segment; Plex's
    server-issued media paths append below the configured base, preserve escaped bytes, and reject
    normalized dot-segment escape with a fixed peer-path-free error.
- [x] Cover DAAP malformed nested containers and session expiration. The DAAP socket fixture reads
  through `CRLFCRLF` in forced seven-byte fragments under a 16 KiB header cap and five-second
  deadline, and owns/cancels spawned handlers through a `JoinSet`. Nine catalogue cases cover a
  wrong top-level container, a missing/wrong listing container, truncated top-level and nested
  framing, a valid `mlit` prefix followed by a malformed remainder, nesting beyond the parser
  limit, short and overlong integer status fields, and duplicate `mstt`. Every known DMAP integer
  type now requires its exact width rather than accepting a valid prefix or treating malformed data
  as Raw. Eight post-login expiration cases cover HTTP 401 and 403 independently on update,
  databases, and items plus in-band item `mstt` 401 and 403; an explicit `mstt` 500 proves
  non-authentication classification. Each case has a five-second test deadline and fixed typed
  error. The tests drive `DaapBackend::connect` directly through a real connection attempt and
  prove it returns no backend, issues no stream/artwork request, and performs exactly one automatic
  bounded logout. Separate registry lifecycle tests prove that only an explicitly retained
  successful attempt installs a source, replacement logs out the displaced session, and
  shutdown/release races remain joined and exactly-once.
- [x] Record implementation: source commits `b80e534` and `6f6c9ac` provide the shared service and
  DAAP foundations; this completion branch adds the representative behavior matrix and fixes the
  Subsonic peer-message, Radio-Browser/geolocation status, and Jellyfin/Plex reverse-proxy findings
  it exposed (PR #118).

Protocol decision and compatibility boundary: GNOME's primary `libdmapsharing` implementation
[returns HTTP 403 for an invalid DAAP session on database routes](https://gitlab.gnome.org/GNOME/libdmapsharing/-/blob/de7af2940a7cfbe626fa063dd89dab41e04170ce/libdmapsharing/dmap-share.c#L605-608),
[emits HTTP-shaped `mstt` values in successful DMAP responses](https://gitlab.gnome.org/GNOME/libdmapsharing/-/blob/de7af2940a7cfbe626fa063dd89dab41e04170ce/libdmapsharing/dmap-share.c#L745-825),
and [its client rejects an explicit non-200 `mstt`](https://gitlab.gnome.org/GNOME/libdmapsharing/-/blob/de7af2940a7cfbe626fa063dd89dab41e04170ce/libdmapsharing/dmap-connection.c#L900-936).
Tributary therefore treats either HTTP 401/403 or in-band `mstt` 401/403 as an expired/unauthorized
session, maps any other explicit non-200 `mstt` to a connection failure, and continues to accept
an absent `mstt` on login or later session responses for older peers; server-info retains its
pre-existing mandatory-success-status rule. A present status must be one exact-width U32 and unique
within its response container. The shared HTTP classifier covers every post-login session-bound
control or catalogue route; once login yields a usable session ID, any later failure attempts one
bounded logout before returning. This closes the DAAP malformed/session-expiration portion of the
now-complete representative cross-service behavior matrix.

Completion-branch validation: debug and release each pass 20 library, 757 application, and 10
repository-metadata tests (787 per profile); strict all-target/all-feature Clippy passes in both
profiles; formatting and whitespace checks pass. The behavior matrix adds 12 tests to the accepted
service/DAAP foundation and was accepted in PR #118 after rebasing onto PR #117.

### P3.4 Add UI/output integration harnesses

- [x] Cover GTK list-item recycling and stale callback prevention. Each sidebar row installs one
  parameterized `GAction` during factory setup. `bind` replaces its immutable `(kind, source)`
  target and `unbind` installs a no-authority target before hiding the button, so neither signal
  accumulation nor a captured prior `SourceObject` can survive recycling. A display-independent
  harness activates that exact production action across repeated manual-source binds, explicit
  unbind, connecting/no-action state, a different connected DAAP source, and the Playlists header;
  it proves one exact delete, disconnect, or menu action and no historical callback on each step.
- [x] Cover playback-session behavior across sorting/filtering/navigation. Queue capture now has
  one production `ListModel` boundary shared by every track-list start. Direct initial, Next,
  Previous, EOS replay, and rejected-load retry take the URI from the session's current immutable
  queue item, assign a fresh output-event generation, and retain a synchronously rejected item for
  a newer retry. The headless harness captures local A/B/C at B, replaces the live projection with
  descending C/B/A, filters B out, then navigates the projection to a remote source. It drives the
  real session/output seam through B→C→B and proves identity remains local, the first B event is
  stale after Next, the remote projection never retargets Previous, and Stop clears ownership
  before invoking the output.
- [x] Cover output transfer and reselect semantics. Output selection is now one validated slot
  transaction: exact endpoint reselection returns before queue, generation, output, or parking
  mutation; a real change validates the replacement's output type and the invariant that Local is
  parked only while a remote target is active, clears session ownership before Stop, parks the
  exact Local output, drops a displaced remote output, and restores Local without the former dummy
  MPD worker. The fake-output harness proves current identity and generation survive reselection,
  a Local→MPD transition rejects a stale Local event, a refused remote current-item load retries
  the same URI under a newer generation, MPD→Chromecast stops/drops the displaced remote while
  retaining parked Local, Chromecast→Local restores that exact instance, and a mismatched
  replacement or wrong-type parked slot preserves the active output and queue unchanged.
- [x] Cover fake MPD and delayed/adversarial Chromecast state machines, including a cap applied
  before allocating from a peer-advertised Cast frame length. The existing 83-test MPD harness
  covers its ordered worker, ownership and targeted cleanup, held-ACK FIFO behavior, bounded and
  cancellable resolution, slow-greeting address fallback, real IPv6, protocol bounds, terminal
  proof, stale generations, and poisoned-session retirement. The existing delayed/fake/real-peer
  Chromecast harness plus this slice totals 42 focused tests and covers ordered effects, stale
  intents, synchronized cleanup, semantic and poisoned failures, silent TLS peers, absolute
  trickle deadlines, Stop/replacement/Shutdown progress, protected request containment, and frame
  adversaries. A framing-aware `Read + Write` adapter now sits above the worker-local rustls stream
  and below the real `rust_cast 0.21` `MessageManager`. It buffers the complete big-endian header,
  rejects lengths above 1 MiB before exposing any header byte to upstream's
  `Vec::with_capacity(length)` path, passes accepted bytes and all writes through unchanged,
  enforces the accepted payload boundary, and resets only after that exact payload completes. Four
  real-manager regressions prove an oversized header is rejected without reading its sentinel
  payload, an exactly 1 MiB valid protobuf frame is accepted, partial headers and payloads fail
  with `UnexpectedEof`, and fragmented consecutive frames reset correctly while manager writes
  retain their framing. The cap is deliberately generous because Cast V2 carries protobuf control
  messages rather than media bytes.
- [x] Cover stale album-art and source-result generations. A bounded real-HTTP fixture pauses the
  first request after the persistent production artwork worker has accepted it, queues a second
  generation, then releases the old response. The old completion channel closes without bytes and
  only the current body crosses the worker boundary; the existing GTK callback repeats the same
  generation check before texture publication. A second harness drives the production source
  cache/eviction boundary with reversed same-key completion order, proving a stale loaded or
  missing result cannot replace or remove the newer projection, while the newest result for an
  inactive source is cached without rendering. Accepted owned image buffers are transferred into
  `glib::Bytes` without copying them a second time.
- [x] Add keyboard context-menu and slider accessibility checks. This slice routes unmodified Menu
  and exact Shift+F10 through the right-click selection snapshot, consumes
  only a shortcut that opens a non-empty menu, exposes `has-popup` plus the standardized
  `Shift+F10 ContextMenu` property on the track list, and explicitly labels and orients the position
  and volume sliders in all 13 catalogs. A production-consumed interaction plan pins the bubbling
  key controller, empty/non-empty propagation, immutable post-popup selection snapshot, and
  popover-scoped action ownership. Focused tests reject Shift+Menu, plain F10, and unrelated
  chords, ignore ambient CapsLock and legacy NumLock/Mod2 state, exercise that complete plan, and
  parse each YAML catalog to prove both keys exist rather than mistaking an English fallback for a
  valid translation.
- [x] Record implementation: PR #119. The final lifecycle slice adds
  two net-new application tests (three focused integration tests after replacing the old pure
  output-decision assertion) and completes the production-boundary evidence recorded below. With
  the earlier recycling, MPD/Chromecast, stale-result, and accessibility slices, all seven P3.4
  checklist items are complete.

The checked accessibility slice deliberately relies on `GtkScale` for slider focus, arrow/Page
Up/Page Down/Home/End behavior, role, current value, and range, then supplies the application-owned
localized name and horizontal orientation. The context menu retains one action group on the
one-shot popover rather than on the long-lived list, so closing it releases the captured selection;
keyboard activation and pointer activation cannot drift into separate action implementations.
P3.4 is complete. Its deterministic tests deliberately stop at the headless production boundaries:
compositor-dependent synthetic key injection, physical receiver behavior, and audible output remain
installed/manual validation rather than being inferred from unit-process success.

### P3.5 Make coverage reporting representative

- [x] Stop excluding all UI, remote backends, migrations, desktop integration, and `main.rs`
  from the only coverage report. CI now runs one comprehensive Linux x86_64 aggregate over every
  host target and feature; the active filename-exclusion regex is removed from CI and all three
  developer build helpers. Repository contract tests fail if an exclusion returns or any helper
  silently narrows the source set.
- [x] Split pure unit and integration coverage if platform constraints require it. No split is
  required inside the canonical Linux host: its unit, binary, repository integration, and
  property-based suites run together. Developer helpers use their active compiler and other
  architectures' conditional code cannot produce a comparable percentage on Linux x86_64, so
  every helper summary is explicitly informational while native CI continues to compile and test
  those source sets.
- [x] Establish a documented baseline and ratchet policy. The dedicated job pins Rust 1.92.0,
  `llvm-tools-preview`, cargo-llvm-cov 0.8.7, `Cargo.lock`, all targets, and all features, then
  enforces the reviewed numeric value in `coverage-baseline.txt`, attempts the HTML upload on every
  outcome, and treats a missing artifact as an error. README documents the two-clean-run,
  lower-result, round-down-minus-0.1 repository review policy. CI enforces the checked-in floor but
  does not compare it with the base branch; ordinary PRs retain or raise it, while a decrease
  requires a dedicated measurement-definition change and rationale. Two complete local
  source-instrumentation runs measured 67.49% and 67.50% production lines after applying
  cargo-llvm-cov's documented default omission of test-source files. The first exact pinned run
  (GitHub Actions run 29595891966) then passed all 746 tests, generated the 80-file HTML artifact,
  and measured 67.03% lines (32,909/49,099), 66.84% functions (2,870/4,294), and 65.93% regions
  (51,397/77,952). It failed solely because that exact denominator was below the provisional 67.3%
  local floor. The clean repeat in run 29596583439 measured 67.02% lines (32,904/49,099), 66.81%
  functions (2,869/4,294), and the same 65.93% regions (51,392/77,952), passed the gate, and uploaded
  another 80-file report. The lower exact result rounds down to 67.0%; subtracting 0.1 confirms the
  reviewed 66.9% floor.
- [x] Record implementation: PR #111; commits `fbd7e7d` and `a8d41aa`; exact pinned coverage
  acceptance in runs 29595891966 and 29596583439.

Acceptance criteria: the sole comparable report includes every production source file compiled on
Linux, uses a version-pinned compiler/frontend and locked Rust dependency inputs, fails below a
reviewed nonzero line floor, and leaves an HTML artifact when the line threshold fails. Different
platform source sets remain visible without being misrepresented as directly comparable
percentages.

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

Current branch validation (2026-07-18, P3.1 Radio-Browser registry/view adapter):
`cargo check --locked --all-targets --all-features`, strict all-target/all-feature Clippy in debug
and release, `cargo fmt --all -- --check`, and `git diff --check` pass. Locked complete debug and
release suites each pass 20 library, 895 application, and 10 repository-metadata tests (**925
total**). Focused evidence includes 53/53 generic lifecycle tests, 14/14 source-registry tests, 9/9
media-boundary tests, and 37/37 radio-filtered client/adapter/UI/playback tests. Adversarial coverage
proves exact cross-view greatest-generation selection despite out-of-order completion, same-view
replacement, view removal, disconnect, and last-registry-drop revocation; valid public query data;
weak authority that cannot retain the registry; cancellation-biased view loads; partial and
successful-empty Near Me tiers; tier-precedence deduplication before stable distance ordering;
pathless queue capture and no direct-URI bypass; accepted-empty versus failed-refresh GTK behavior;
selected-before-publication source loss; and exact active-lane failure ownership. Independent
integrated review found and fixed a first-use Near Me race: an exact generation-owned consent
prerequisite now prevents unrelated lifecycle invalidations from treating the open dialog as source
loss, while stale/superseded requests cannot suppress real fallback. This slice closes another part
of P3.1's compound final record without changing literal arithmetic: **219/223 (98.2%)** overall,
**76/79 P2**, and **29/30 P3**; removable and external-file adapters remain.

PR #125 validation (2026-07-18, P3.1 retained local embedded-art authority):
`cargo check --locked --all-targets --all-features`, strict
`cargo clippy --locked --all-targets --all-features -- -D warnings` and
`cargo clippy --release --locked --all-targets --all-features -- -D warnings`,
`cargo fmt --all -- --check`, and `git diff --check` pass.
`cargo test --locked --all-targets --all-features` and its `--release` counterpart each pass 20
library, 872 application, and 10 repository-metadata tests (**902 total**). The 9 focused album-art
tests cover retained-handle tag parsing, path replacement, authority drift, shared-cursor rewind
behavior, stale generation rejection, and bounded/checked raw MP4 fallback. This closes only the
embedded-art portion of P3.1's compound final record, so the tracker remains **219/223 (98.2%)**
overall, **76/79 P2**, and **29/30 P3** while Radio-Browser, removable, and external-file adapters
remain.

Accepted validation (2026-07-18, PR #124 P2.11 Windows PE-import named-binding follow-up):
`cargo check --locked --all-targets --all-features`, strict all-target/all-feature Clippy in debug
and release, `cargo fmt --all -- --check`, and `git diff --check` pass. Locked full debug and release
suites each pass 20 library, 866 application, and 10 repository-metadata tests (**896 total**),
including all socket-bearing regressions run serially. All 11 focused Windows platform-runtime
contracts pass. PowerShell 7's parser accepts the complete `scripts/build-windows.ps1`; this is a
syntax check, not a claim that PowerShell 7 reproduced the affected Windows host's behavior.
The strengthened regression extracts the exact typed `Invoke-BoundedPeImportBatch` declaration,
finds exactly two production logical invocations, and requires each invocation independently to
bind all seven unique names with the exact singleton or batch path value. An initial locked-check
attempt exhausted this shared runner's `/tmp` quota while compiling `aws-lc-sys`; removing only
stale build outputs from two already-merged worktrees and rerunning the same command passed. No
source workaround, dependency, lockfile, packaging-policy, or checklist change resulted. Gemini's
review found no issue, CodeQL passed, and CI run `29633729566` passed every job. Both native Windows
architectures completed the production bundle/probe path with the named calls. The user's affected
Windows PowerShell/MSYS2 rerun has since reproduced the singleton Soup-target failure, proving this
CI/named-binding change is insufficient; a second packaging repair and another exact-host rerun are
required before the separate live packaged-playback task can close.

Accepted validation (2026-07-17, PR #114 P2.11 packaged-probe teardown hardening):
`cargo check --all-targets --all-features --locked`, strict all-target/all-feature
Clippy, and `cargo test --all-targets --all-features --locked` pass in debug and release. Each full
test profile passes 18 library, 731 application, and 10 repository-metadata tests (759 total), and
all 28 focused platform-runtime tests pass in both profiles. The four new regressions live in the
Windows-only packaged runtime module; this Linux validation parses and formats them but does not
claim to compile or execute them. The x86_64 Windows job runs unit
tests; ARM64 skips that step but compiles the production module and runs the bundled executable's
production probe during packaging. The tests separate incomplete-header EOF/reset/abort from
semantic drift, synchronize on media acceptance, exercise the production begin/NULL/finish ordering,
require malformed input completed after cancellation begins to remain fatal, and prove the poison
observer remains live until final stop drains queued accepts. Formatting and whitespace checks
(`cargo fmt --all -- --check` and `git diff --check`) pass; no dependency, lockfile, README,
checklist, or completion-count change is involved. Run 29610563120 passed its completed static,
audit, metadata, coverage, Linux aarch64, and Flatpak siblings, but Windows x86_64 strict Clippy
rejected the initial eight-parameter listener before native tests. The follow-up groups the two
lifecycle atomics into one state object without weakening their separate meanings. Static run
29611191885 passed, and replacement matrix 29611194118 passed every other completed job, including
the ARM64 Windows production package probe;
the x86_64 native suite passed 717 of 718 application tests but its plain client-socket drop did not
deterministically deliver the intended incomplete-header EOF. That regression now explicitly
half-closes the client write side after cancellation—the same EOF contract exercised by the
already-passing request classifiers. Static run 29612456247 passed, but matrix 29612458758 proved
the half-close alone insufficient; the malformed fixture also received an abort while draining its
response (716 of 718 application tests passed). The production
listener now explicitly returns each accepted Winsock socket to blocking mode before installing
its bounded read/write deadlines, so a fragmented header waits rather than spuriously returning
`WouldBlock`; configuration, timeout, semantic-request, and response failures remain fatal. The
malformed fixture now half-closes its completed request before response drain to make completion
deterministic. Final static run 29613604485 and complete matrix 29613606936 passed every job,
including all 718 x86_64 Windows application tests and both native finished-distribution probes.
No actionable automated review thread remains; Gemini posted only its service-sunset notice.
Accepted validation (2026-07-17, PR #115 P2.11 Windows distribution-path follow-up):
the
PowerShell helper parses without AST errors; strict all-target/all-feature Clippy passes in debug
and release; and `cargo test --all-targets --all-features --locked` passes in debug and release.
Each profile passes 18 library, 733 application, and 10 repository-metadata tests (761 total),
including all 30 focused platform-runtime tests. The two net-new path regressions also pass
independently. One static contract retains caller-relative
`dist\tributary-windows` creation and requires physical-provider canonicalization before singleton
Soup import inspection. The executable regression changes PowerShell `$PWD` to a repository path
containing spaces while leaving the process working directory elsewhere, reproduces the old
divergent `Path.GetFullPath` result, and proves the fixed path is absolute and existing. It then
mounts that repository beneath a custom FileSystem PSDrive and proves `ProviderPath`, rather than
the provider-only drive-qualified `Path`, reaches the same physical directory. Formatting and
`git diff --check` pass. No dependency or lockfile changed and no checklist box moved. CI run
29615869107 passed MSRV, audit, metadata, representative coverage, Linux x86_64/aarch64, Flatpak,
macOS, both native Windows builds and finished-distribution probes, packages, and checksums;
CodeQL run 29615866970 passed every analysis. No actionable automated review thread remains;
Gemini posted only its service-sunset notice.

Final PR #119 branch validation (2026-07-17, complete P3.4 stack rebased onto PR #118): strict
all-target/all-feature Clippy and `cargo test --all-targets --all-features --locked` pass in debug
and release. Each full profile passes 20 library, 767 application, and 10 repository-metadata tests
(797 total). Three focused lifecycle integration regressions pass directly in debug and in both full
profiles; they replace one pure output-decision test and add two net-new application tests. The
playback regression crosses the production `ListModel` capture, immutable queue cursor, direct
`AudioOutput`, and Stop boundaries while the projection is reordered, filtered, and replaced by a
different source. It proves exact current identity, B→C→B navigation from the original snapshot,
per-load generation ownership, stale-event rejection, and clear-before-Stop cleanup without a GTK
display or device. The output regression crosses the production target/output/session/parked-local
transaction with recording Local, MPD, and Chromecast outputs. It proves reselection is a complete
no-op, Local is parked and restored exactly, remote rejection remains retryable under a newer
generation, remote-to-remote and remote-to-Local transfer clear before Stop and drop the displaced
output, and a wrong-type replacement cannot mutate current playback. Output construction now uses
the requested typed target directly
and no longer creates a throwaway MPD worker merely to move Local into its parked slot. Formatting
and `git diff --check` pass. Automated review additionally caught and the branch now covers ambient
NumLock/legacy modifier bits in keyboard-menu matching, and artwork publication transfers its
owned byte vectors into GLib without an avoidable full-buffer copy. These review fixes changed no
dependency, lockfile, locale, protocol, schema, package version, packaging, or release-workflow
file. These last two harness boxes plus the implementation
record complete P3.4, advancing the literal total to **216/223 (96.9%)** and P3 to **26/30**.

Pre-rebase parent validation (2026-07-17, P3.4 recycled-row/UI-generation slice):
strict all-target/all-feature Clippy and `cargo test --all-targets --all-features --locked` pass in
debug and release. Each full profile passes 18 library, 743 application, and 10
repository-metadata tests (771 total). The three focused production-boundary regressions pass
directly in debug: the exact row-lifetime `GAction` is driven across repeated bind/unbind targets;
a synchronized loopback server holds and releases an in-flight persistent-worker artwork fetch
across a newer request; and reversed same-key loaded/missing results cross the source-cache and
eviction boundary without altering the newer projection. These replace the old closure-shaped
sidebar model with the actual production dispatcher and add two net-new tests. The artwork fixture
uses channel synchronization and bounded socket I/O rather than sleeps or elapsed-time thresholds,
and the GTK-independent action core runs on every native headless test host. Formatting and
`git diff --check` pass. No dependency, lockfile, locale, package version, protocol, packaging, or
release-workflow file changed. Exactly the list-item recycling/stale-callback and stale
artwork/source-result boxes closed at that staged point. PR #119's final validation and accepted
aggregate arithmetic supersede this historical stack-local inventory.

Pre-rebase base validation (2026-07-17, combined P3.4 accessibility/Cast slice): strict
all-target/all-feature Clippy and `cargo test --all-targets --all-features --locked` pass in debug
and release with two build jobs. Each full profile passes 18 library, 741 application, and 10
repository-metadata tests (769 total). The six net-new focused regressions also pass directly: two
pin the accepted/rejected Menu and Shift+F10 interaction plan plus exact slider labels in every
source YAML catalog, and four exercise the real `rust_cast 0.21` manager at oversized, exact-limit,
truncated, and consecutive-frame boundaries. Both full profiles include all 83 focused MPD tests
and all 42 focused Chromecast tests. Independent review found no issue with the intended 1 MiB
control-frame policy or its pre-allocation, deadline, and poisoned-session boundaries. Formatting
and `git diff --check` pass. `serde_yaml`, already locked transitively, is now an explicit
test-only dependency; no production dependency, package version, protocol schema, packaging, or
release workflow changed. Exactly the keyboard/context-menu/slider accessibility box and the
compound MPD/Chromecast harness box closed at that staged point. PR #119's final validation and
accepted aggregate arithmetic supersede this historical stack-local inventory.

Base local branch validation (2026-07-17, P3.2 shared-catalogue completion): strict
all-target/all-feature Clippy and `cargo test --all-targets --all-features --locked` pass in debug
and release. Each full profile passes 18 library, 735 application, and 10 repository-metadata tests
(763 total). The trait-object catalogue spy and paired passwordless-DAAP error/cleanup helper tests
also pass individually in both profiles, while PR #114's four local aggregate regressions remain
green in the complete suite. The production diff constructs `LocalBackend` for scanner snapshots,
routes every complete local and remote catalogue publication through the explicit
`&dyn MediaBackend` adapter, removes all backend-specific `all_tracks` bypasses, and preserves the
concrete authentication/session boundaries assigned to P3.1. Formatting and `git diff --check`
pass. No dependency, schema, migration, or lockfile changed. Independent review found no code
integration or arithmetic defect after the rebase; its documentation findings were corrected.
The two P3.2 closures advance the literal total to **205/223 (91.9%)** and P3 to **15/30**; native
and package validation remains required in PR CI.

Earlier accepted validation (2026-07-17, PR #112 P2.10 exclusive-control slice):
`cargo check --all-targets --all-features --locked`, strict all-target/all-feature
Clippy, and `cargo test --all-targets --all-features --locked` pass in debug and release. Each full
profile passes 18 library, 727 application, and 10 repository-metadata tests (755 total); all 83
focused MPD tests pass. Nine net-new regressions cover legacy default-false deserialization and
approved-mode serialization, exact in-place upgrade with name/sibling preservation, repeat-upsert
deduplication, mode-sensitive output identity, worker-level zero-connection/state/ticket/queue
action for protected and pre-rejected loads, public-boundary refusal before Buffering/epoch/enqueue
for direct and typed media, exact-generation retry state, and warning/confirmation/failure
localization across all 13 catalogs. The first restricted release run denied every loopback bind at
the test environment boundary; an immediate rerun with loopback access passed all 755 tests.
`cargo audit` reports no vulnerability and only the two
tracked allowed unmaintained warnings; desktop and AppStream validation,
`cargo fmt --all -- --check`, and `git diff --check` pass. No dependency or lockfile changed. The
exact pinned PR run `29602279148` passed MSRV, audit, metadata, representative coverage, Linux
x86_64/aarch64, Flatpak, macOS aarch64, Windows x86_64/aarch64, and checksum jobs. Its 49,451-line
report covered 33,191 lines (67.12%, artifact `8415577394`) against the accepted 66.9% floor.
CodeQL and all three static-analysis jobs passed; final Codex review of `91536ab` reported no
issues after both earlier findings were fixed and resolved. P2.10 is complete at 198/223 overall
and 76/79 P2; P3.5's exact-toolchain acceptance remains recorded by PR #111.

PR #121 local branch validation (2026-07-17, P3.1 local-authority review follow-up):
Gemini correctly identified that the resolver's post-acquisition whole-model comparison included
`library_roots.last_checked_at`, even though ordinary successful scans refresh that observational
timestamp and the existing root-trust contract explicitly excludes it from security state. A
named expected-authority snapshot now compares only `path`, `device_id`, `identity_confirmed`,
`is_available`, and `last_scan_complete`; exact track `file_path` comparison remains separate.
One deterministic regression proves timestamp-only drift is accepted and independently changes
every authority field to prove each still rejects. The focused regression passes in debug and
release. `cargo test --all-targets --all-features --locked` passes in debug and release with 20
library, 804 application, and 10 repository-metadata tests (834 total) per profile. Strict
all-target/all-feature Clippy passes in debug and release; formatting and diff checks pass. The
completed-box count at that milestone was 218/223 and P3 was 28/30.

Earlier local branch validation (2026-07-17, P3.1 stable-identity independent-review follow-up):
`cargo check --all-targets --all-features --locked`, strict
`cargo clippy --all-targets --all-features --locked -- -D warnings`, and
`cargo test --all-targets --all-features --locked` pass in debug and release. Each complete profile
runs 20 library, 797 application, and 10 repository-metadata tests (827 total). The 49-test identity filter
passes together with the production non-resolvable-origin/loopback-route promotion regression,
exact native Subsonic/Jellyfin/Plex/DAAP assertions, saved-ID collision quarantine, production-queue
capture, recycled-sidebar action, DAAP adversarial/expiration, and already-connected
reconnect-publication regressions. `cargo fmt --all -- --check` and `git diff --check` pass.
The aggregate CodeQL gate then classified the promoted-route test's mock password literal as a new
critical hard-coded credential. The regression now generates a disposable UUID secret at runtime;
this is test-only scan hardening and does not change production authentication or the test's route
assertions. The focused regression and strict validation were rerun after that correction.
Gemini's follow-up review found one valid avoidable Radio-Browser ID copy, now removed without
changing accepted or quarantined rows. Its suggestion to accept uppercase reference hex was not
applied: the frozen encoder emits lowercase only, canonical spelling is part of the opaque-ticket
contract, and `malformed_and_wrong_kind_references_fail_closed` already asserts uppercase `id-2E`
is rejected. Both review threads were resolved after recording that distinction.
Codex then found that a failed environment-configured reconnect emitted only a generic error after
marking a reused saved row as connecting. The newest standard and DAAP environment attempts now
emit an exact-source, opaque-UI-token failure transition after both authentication and catalogue
failures; GTK clears only while that token still owns the row's spinner, preserves an
already-connected predecessor, and leaves unrelated rows untouched. Attempts superseded before
failure emit nothing, while a failure queued before a same-source retry is rejected after that
retry replaces the UI token. Generic/manual connecting transitions also invalidate an old
environment token. The new reordered same-source/exact-owner regression, complete 827-test debug
and release suites, strict Clippy in both profiles, formatting, and whitespace checks pass after
the correction. An independent recheck of the strengthened token ownership and all eight producer
branches found no additional actionable issue. The first Linux CI execution then exposed that the
legacy-migration failure fixture depended on removing parent-directory write permission, which is
not a failure injection when the container runs as root. The fixture now supplies a deterministic
atomic-save error through the loader's private persistence seam and asserts the same quarantined
result and byte-for-byte unchanged legacy file on every platform. This adds the regression to
Windows rather than changing the Linux total or any production migration path; the focused test
passes in debug and release, and the checklist arithmetic remains unchanged.

Pre-merge local branch validation (2026-07-17, P3.1 stable-identity runtime before PR #120):
`cargo check --all-targets --all-features --locked`, strict all-target/all-feature Clippy in debug
and release, and `cargo test --all-targets --all-features --locked` in debug and release pass. Each
full profile passes 18 library, 752 application, and 10 repository-metadata tests (780 total).
Focused suites pass for identity encoding, saved-source migration/quarantine, source objects and
navigation, standard/DAAP registry ownership, exact native backend conversion/resolution,
local-ID-at-use resolution, queue/view ownership, radio, removable scanning, and recycled sidebar
actions. Independent review regressions additionally prove reserved saved IDs quarantine without
letting a remote impersonate local; persistence adopts an existing discovered row's ID before its
in-place promotion, repeated Add and saved-plus-env startup retain their persisted owner; and stream
plus artwork references round-trip exact `.` and `..` IDs without URL normalization. The first
full debug run exposed one stale assertion that still expected a server URL from a sidebar
action after production moved to stable source ownership; the corrected regression now expects the
persisted/derived `SourceId`, and the complete debug and release suites pass afterward.
`cargo fmt --all -- --check` and `git diff --check` pass. No dependency or lockfile changed.

Earlier local branch validation (2026-07-17, P3.5 representative-coverage slice before CI):
Accepted validation (same PR #114, P3.2 stable-local-aggregate slice):
`cargo check --all-targets --all-features --locked`, strict
`cargo clippy --all-targets --all-features --locked -- -D warnings`,
`cargo fmt --all -- --check`, and `git diff --check` pass. Four focused SQLite backend tests cover
the two pinned UUID goldens, artist/album domain separation, collision-safe framing, exact metadata
and Unicode-whitespace fallback, same-title disambiguation, compilation grouping, deterministic
year/genre minima, aggregate counts/stats, listing-ID round trips, deterministic result ordering,
unknown IDs, and an empty library. The full local-module surface passes 243 tests. After `b067d18`,
the `5012b77` follow-up replaced each full-table lookup with compact aggregate-key resolution and
an exact title/artist SQL query. The complete current debug all-target/all-feature suite then passed
18 library, 731 application, and 10 repository-metadata tests (759 total), in addition to the
focused and local-module runs above. No dependency, schema, migration, or lockfile changed.
Static analysis and CodeQL passed in run 29607279056. Run 29607280861 passed MSRV, audit, desktop
metadata, representative coverage, Linux x86_64/aarch64, Flatpak, macOS aarch64, Windows
x86_64/aarch64, package creation, and SHA256 checksums. The review window produced no actionable
code comments; Gemini posted only its service-sunset notice. The four local backend regressions
exercise only SQLite and do not claim the still-unused production `LocalBackend` integration
boundary. Documentation-head run 29608292265 passed release tests and every completed sibling but
failed the debug Linux suite on the MPD terminal enqueue/wake race documented above. After the
linearization fix, the failing regression and all 83 MPD tests pass in both debug and release;
formatting and whitespace checks pass. Final static run 29614521885 and complete replacement
matrix 29614525132 passed every job, as recorded above.

Previous local branch validation (2026-07-17, P3.5 representative-coverage slice before CI):
`cargo fmt --all -- --check`, strict all-target/all-feature Clippy in debug and release, and
`cargo test --all-targets --all-features --locked` in debug and release pass. Each profile passes
18 library, 718 application, and 10 repository-metadata tests (746 total); the two new metadata
tests pin the coverage job's toolchain, command, baseline, artifact, and every developer helper.
Both shell helpers pass `bash -n`, the Windows helper parses without PowerShell AST errors, the CI
workflow parses as YAML, and `git diff --check` is clean. With cargo-llvm-cov and Rust 1.92's LLVM
tools unavailable locally, two complete runs using the already-installed Rust/LLVM source
instrumentation measured 67.49% and 67.50% production lines after cargo-llvm-cov's documented
test-source omission; no tool was installed or downloaded to manufacture local acceptance. The
first exact pinned PR-CI run (29595891966) subsequently passed the same 746 tests and generated an
80-file report covering the formerly excluded UI, remote backends, radio, migrations, desktop
integration, and entry point. It measured 67.03% lines, 66.84% functions, and 65.93% regions, then
failed only against the provisional 67.3% local floor. The repeat exact pinned run 29596583439
passed the same test and artifact gates and measured 67.02% lines, 66.81% functions, and 65.93%
regions. The lower exact result confirms the checked-in 66.9% floor under the documented policy.
CI prints that exact summary while preserving the original test/threshold exit status.

Previous branch validation (2026-07-17, PR #110 packaged-Windows P2.11 slice before CI):
`cargo check --all-targets --all-features --locked` and
`cargo test --all-targets --all-features --locked` pass in debug and release, as does strict
all-target/all-feature Clippy in both profiles. Each full profile passes 18 library, 718
application, and 8 repository-metadata tests (744 total); all 28 focused platform-runtime tests
also pass in debug and release. Three Windows-gated request-classification tests separately pass
under a local test gate and will run natively in PR CI. The packaged probe directly preflights its
exact scanner, builds a new external registry, verifies required factory/decoder provenance,
decodes the real FLAC fixture through the production protected-source policy to a handoff and EOS,
proves zero poisoned-proxy connections, and exercises the fixed alternate-source failure. Review
hardening arms the listener window after cold discovery, makes every invalid media request fatal,
co-locates the scanner with its shipped DLLs while removing probe-only DLL search help, retains
Windows PowerShell 5.1 process/argument fallbacks, and replaces executable dependency discovery.
ARM64 CI first exposed a missed path-only Soup dependency; after that parser fix, its next package
run showed the fundamental problem by hanging for more than 33 minutes while `ldd` executed
`libgstencoding.dll`. The accepted fix replaces executable inspection with bounded batched PE
import-table reads, while retaining the exact recursive copy queue and direct Soup edge gate. It
also bounds the dependency closure, scanner termination, process-tree termination,
redirected-output drain, diagnostics, and overall execution.
PowerShell parses without errors; `cargo audit` reports no vulnerability and
only the two tracked allowed unmaintained warnings; desktop and AppStream validation,
`cargo fmt --all -- --check`, and `git diff --check` pass. No dependency or lockfile changed. CI run
`29593455545` passed every required check; its native Windows x86_64 (14m15s) and ARM64 (12m17s)
jobs both completed the architecture-local PE-import closure, exact packaged scanner preflight,
fresh external registry, bundle-only factory/decoder provenance, protected FLAC decode/EOS,
direct/zero-retry/30-second source policy, zero poisoned-proxy connections, alternate-source
fail-closed path, and ZIP creation.

Most recent branch validation (2026-07-17, PR #109 P2.11 real-GStreamer fake-backend slice):
`cargo check --all-targets --all-features --locked` and
`cargo test --all-targets --all-features --locked` pass in debug and release, as does strict
all-target/all-feature Clippy in both profiles. Each full profile passes 18 library, 706
application, and 8 repository-metadata tests (732 total). The one net-new process-isolated
regression builds real DAAP and Subsonic requests beneath a reverse-proxy prefix and sends both
sequentially through the production `Player`, per-load app-owned proxy, `souphttpsrc`, FLAC decoder,
and `fakesink` to a decoded-buffer handoff and exact-generation EOS. Its upstream fixture accepts
ordinary and byte-range fetching while validating exact paths and query multisets, DAAP's four
protocol headers, absence of `Authorization`, `Proxy-Authorization`, `Cookie`, and `Referer`, and
at least one body GET per backend. It separately observes the post-policy source properties and
proves the GStreamer URI remains an opaque loopback ticket with no private request value. Missing
`playbin3`/`playbin`, `souphttpsrc`, `fakesink`, or FLAC decoding fails the child rather than
silently skipping; per-load and absolute parent deadlines contain native hangs, and a success
sentinel prevents an incorrect exact-test filter from passing with zero tests. Automated-review
follow-ups make sentinel cleanup panic-safe through an RAII guard and fail immediately if the
player event channel closes unexpectedly instead of waiting for the case deadline.
`cargo audit` finds no vulnerability and only the two tracked allowed unmaintained warnings;
desktop and AppStream validation, `cargo fmt --all -- --check`, and `git diff --check` pass. No
dependency or lockfile changed.

Previous branch validation (2026-07-16, PR #108 P2.11 deterministic HTTP-compatibility slice):
`cargo check --all-targets --all-features --locked` and
`cargo test --all-targets --all-features --locked` pass in debug and release, as does strict
all-target/all-feature Clippy in both profiles. Each full profile passes 18 library, 705
application, and 8 repository-metadata tests (731 total). Seven net-new regressions plus an
expanded request-builder regression prove exact root, non-trailing-slash, trailing-slash, and
already-escaped base-prefix behavior; exact DAAP stream/artwork protocol headers and
single-segment format escaping; disjoint fixed/sensitive header allowlists; trusted stream and
artwork request materialization; typed Subsonic HTTP-200 failed-envelope mapping; and explicitly
proxied protected upstream transport. The real loopback
fixtures exercise the receiver-to-ticket-to-upstream boundary, including receiver-header
filtering, private query application, advertised-route origin retention, and selected-proxy
transport. Automated-review follow-ups apply each trusted header map with reqwest's replacement
semantics, cache the immutable DAAP protocol map for process lifetime, and generate fixture
passwords dynamically so code scanning does not mistake test literals for shipped credentials.
`cargo fmt --all -- --check` and `git diff --check` pass. No dependency or lockfile changed.

Previous branch validation (2026-07-16, PR #107 P2.10 real-socket coverage slice):
`cargo check --all-targets --all-features --locked` and
`cargo test --all-targets --all-features --locked` pass in debug and release, as does strict
all-target/all-feature Clippy in both profiles. Each full profile passes 18 library, 698
application, and 8 repository-metadata tests (724 total); all 79 focused MPD tests pass. Two
net-new real-socket regressions prove a silent first greeting cannot consume the later address's
fair share of one absolute deadline, and exercise numeric `::1` resolution, connection, greeting,
and IPv6 client/peer addresses when the host can bind IPv6 loopback. The first test uses channels
without sleeps or a fragile elapsed-time bound; the second skips only when the initial `::1` bind
is unavailable and makes every subsequent assertion mandatory. `cargo fmt --all -- --check` and
`git diff --check` pass. No dependency or lockfile changed.

Earlier branch validation (2026-07-16, PR #106 P2.10 cancellable-resolver slice):
`cargo check --all-targets --all-features --locked` and
`cargo test --all-targets --all-features --locked` pass in debug and release, as does strict
all-target/all-feature Clippy in both profiles. Each full profile passes 18 library, 696
application, and 8 repository-metadata tests (722 total); all 77 focused MPD tests pass. Nine
net-new resolver regressions cover host-size and character admission, ordered deduplication and
the 32-address cap, absolute-deadline cancellation, result-channel loss, lifecycle-epoch
cancellation, real GIO dispatch on the private context, eight-operation saturation with callback
progress and reusable capacity, late-callback send/drop thread ownership, and GIO IPv4/IPv6
conversion with flowinfo and scope. The existing raw/bracketed numeric-IPv6 case is expanded and
renamed separately. `cargo fmt --all -- --check` and `git diff --check` pass. No dependency or
lockfile changed.

Earlier branch validation (2026-07-16, PR #105 P2.10 bounded-ingress/held-ACK slice):
`cargo check --all-targets --all-features --locked`, strict debug and release
all-target/all-feature Clippy, and `cargo test --all-targets --all-features --locked` in debug and
release pass. Each full profile passes 18 library, 687 application, and 8 repository-metadata tests
(713 total); all 68 focused MPD tests pass. Six new regressions cover exact below-cap FIFO,
saturation-only folding without crossing barriers, deterministic oldest-transient eviction,
new-epoch purge plus late-stale rejection, receiver-loss handling, and a real held-ACK TCP peer
that proves prompt enqueue, no command pipelining, and ordered execution after release.
`cargo fmt --all -- --check` and `git diff --check` pass. No dependency or lockfile changed.

Earlier branch validation (2026-07-16, PR #104 P2.10 ACK/terminal/orphan-semantics slice):
`cargo check --all-targets --all-features --locked`, strict debug and release
all-target/all-feature Clippy, and `cargo test --all-targets --all-features --locked` in debug and
release pass. Each full profile passes 18 library, 681 application, and 8 repository-metadata tests
(707 total); all 62 focused MPD tests pass. Seven new regressions cover strict ACK parsing and
redaction, real-socket absence classification, malformed correlation poisoning, non-absence
cleanup rejection, external-Next terminal proof, foreign-current shutdown retention, and a stale
foreign-status response superseded by replacement Load. `cargo fmt --all -- --check` and
`git diff --check` pass. No dependency or lockfile changed.

Prior branch validation (2026-07-16, PR #102 P2.8 Chromecast-deadline slice): `cargo check
--all-targets --all-features --locked`, strict debug and release all-target/all-feature Clippy,
and `cargo test --all-targets --all-features --locked` in both debug and release pass. Each full
test profile passes 18 library, 674 application, and 8 repository-metadata tests (700 total);
12 focused regressions cover absolute trickle deadlines, real silent TLS peers with Stop,
replacement Load, and Shutdown queued behind them, synchronized versus poisoned cleanup,
supersession during poisoned I/O, deterministic numeric mDNS address selection, unusable and
IPv6-only endpoints, and address replacement ordering. `cargo fmt --all -- --check` and
`git diff --check` also pass. The loopback peers exercise the real TCP/rustls deadline transport
without Chromecast hardware; live receiver compatibility remains useful field validation, not a
prerequisite for the bounded-I/O contract. No dependency or lockfile changed.

Previous branch validation (2026-07-16, PR #101 P2.7 platform-runtime slice): `cargo check
--all-targets --all-features --locked`, strict debug all-target/all-feature Clippy, and strict
release all-feature Clippy pass. Debug and release `cargo test --all-targets --all-features`
each pass 18 library, 662 application, and 8 repository-metadata tests (688 total per profile);
the application count includes 16 focused platform-runtime policy/static regressions. `cargo fmt
--all -- --check`, `bash -n scripts/build-macos.sh`, and `git diff --check` also pass. The full
test rerun required the ordinary host loopback namespace because the restricted command sandbox
rejects local socket binds; the same tests pass there. `cargo audit --no-fetch` reports only the
two already accepted unmaintained warnings. This Linux host cannot execute the
target-gated Windows bundle startup or macOS `gdk-pixbuf-query-loaders`/GStreamer/signature probe.
The portable cache-path tests derive absolute fixtures from the host's temporary directory, so
Windows CI exercises the same containment and loader-validation assertions with native
drive-qualified paths rather than failing early on Unix-shaped literals.
The separate fuzz-workspace lockfile includes the new macOS-only direct dependency, and the exact
CI `cargo fmt` plus strict all-target `cargo clippy --locked` fuzz gate passes against it.
The first two native macOS packaging runs proved signing and bundle construction, then the original
generic validator rejected a standalone quoted cache record without identifying it. Upstream's
record grammar permits standalone quoted empty MIME and extension-list fields, so the validator now
tracks module, info, MIME, extension, signature, and record-separator state rather than classifying
every standalone quoted line as a module. It also normalizes only safe helper-relative records that
resolve to the exact expected set, and reports a sanitized basename for any remaining unexpected
module; focused regressions cover both contracts. PR #101 then passed the signed relocated
first-launch probe and strict post-launch verification in the native macOS packaging job, both
Windows architecture jobs, and all other required checks.

Earlier branch validation (2026-07-16, PR #100 P2.6 packaging/CI slice): exact Rust 1.92
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
repository packaging/desktop/MSRV contract tests, two representative-coverage contract tests,
and an `MSRV (1.92)` compile-proof.
Checked boxes above still record the by-hand run before a milestone; the CI jobs are what catch
regressions automatically. Windows x86_64 and ARM64 are allowed to finish independently, and only
the ARM runner disables setup-msys2's optional package cache after its cleanup path intermittently
failed following a successful install; the separate Cargo cache remains enabled.

## Decisions

Record scope or design decisions here so deferred work is explicit.

- 2026-07-17 — P3.3 uses one deterministic keyed HTTP fixture rather than a FIFO script because
  Jellyfin fetches three item classes concurrently and Subsonic performs bounded unordered library
  walks. A route therefore keys method plus exact path plus a required decoded-query subset, while
  tolerating unrelated generated authentication values. Each route owns a locked reply queue and
  exact call count; the service separately records complete request metadata and all dispatch or
  lifecycle failures. Explicit `finish` both bounds graceful shutdown and verifies no route was
  missed, while `Drop` remains cleanup-only so panic unwinding cannot leak a server task. The first
  slice intentionally tests only one successful production flow per non-DAAP service. It does not
  silently convert that foundation into credit for P3.3's adversarial behavior matrix. DAAP's
  protocol-appropriate raw-socket fixture is integrated separately; together they close the named
  service-mock box while leaving the cross-service behavior matrix open.

- 2026-07-17 — The P3.3 DAAP harness treats authentication/session status as a two-layer wire
  contract. HTTP 401 and 403 both mean the session is unauthorized or expired because a primary
  DAAP implementation uses 403 for an invalid database session. When a successfully decoded
  response carries `mstt`, 401/403 have the same typed meaning and every other non-200 value is a
  typed connection failure; missing `mstt` remains accepted on login and later session responses
  for compatibility with older peers, while server-info still requires success status. A present
  status must be a unique exact-width U32, and every other known integer tag likewise requires its
  exact protocol width. Known nested DMAP containers must consume their entire payload, so even a
  valid item prefix followed by a malformed remainder cannot become a partial catalogue. Update,
  databases, and items share the same HTTP classifier, and one bounded logout is attempted for
  every failure after login has yielded a usable session ID—including before a complete client can
  be returned. Diagnostics are fixed and bounded rather than formatting peer-controlled remaining
  bytes. The fixture proves the failed initial sync returns no backend, issues no media request,
  and sends exactly one logout; separate registry tests pin explicit successful retention,
  displaced-session replacement logout, and joined exactly-once shutdown/release races. This
  closes the DAAP
  malformed-container/session-expiration box and, alongside the non-DAAP fixture, the named
  service-mock box; at that slice, the broader P3.3 behavior matrix and final implementation
  record remained open.

- 2026-07-17 — P3.3's completion matrix is representative rather than the Cartesian product of
  seven behaviors and six services. Each behavior crosses at least one complete production entry
  point, each named service is covered where its protocol has that behavior, and common redirect
  security remains pinned once at the shared client-policy boundary. Service-specific pagination
  is covered independently for Jellyfin and Plex; partial-publication contracts are independently
  covered for Subsonic and Plex; DAAP retains its raw DMAP/session harness; and public
  Radio-Browser/geolocation paths cover redirects, finite deadlines, streaming caps, and provider
  fallback. Test-only policies shorten time/size without substituting a different request or body
  reader. Prefix tests cover API and protected-media construction for all four authenticated
  protocols. A Plex media path appends below the configured base and rejects normalization outside
  it, while a peer's path never enters the fixed error. This evidence closes the matrix and record
  boxes without claiming unsupported alternate-auth modes or every redundant pairing.

- 2026-07-17 — The P3.4 accessibility slice uses GTK's unmodified Menu and exact Shift+F10
  conventions and the same one-shot, selection-snapshotted popover as right-click. Shift+Menu,
  plain F10, and unrelated chords propagate. A recognized shortcut also propagates when the view
  has no usable selection/menu, preventing Tributary from shadowing an ancestor or desktop binding
  without performing an action. The track list publishes `has-popup` and the standardized
  `Shift+F10 ContextMenu` token; each scale retains GTK's native slider behavior and publishes a
  localized name plus horizontal orientation. A production-consumed plan pins controller phase,
  propagation, immutable selection ownership, popover action scope, and accessible properties;
  the catalog test parses every source YAML file to distinguish a present translation from a
  fallback. Compositor-dependent synthetic key injection remains future installed UI smoke
  coverage.

- 2026-07-17 — P3.4 treats output changes as an explicit clear-and-transfer policy, not seamless
  migration of loaded media. The selected endpoint is part of output identity; selecting it again
  is a complete no-op, while selecting another endpoint first validates the concrete output type
  and Local/parked-slot invariant, invalidates the queue generation, stops the displaced output,
  then parks/restores the exact Local instance or drops the old remote. An invalid replacement
  leaves the current target, queue, and output intact. This makes the already-documented P0.4
  “transfer or clear” choice executable at one boundary and avoids constructing the old throwaway
  MPD worker merely to move Local. The playback lifecycle harness consumes the same generic
  `ListModel` interface that production's `SortListModel` presents; it mutates a headless
  `gio::ListStore` to represent sort, filter, and source-navigation projections because creating a
  GTK sort model itself requires initialized display state. The assertion remains at the actual
  queue-capture/direct-output boundary rather than duplicating session rules in a test-only model.

- 2026-07-17 — PR #112 completes P2.10 by requiring an explicit, persisted
  exclusive-control promise rather than pretending MPD offers an ownership lock or conditional
  partition mutation. MPD's pause, stop,
  repeat, random, single, and consume commands are global to the selected playback partition; a
  named partition can still be joined by another client and is not a lock. Legacy saved endpoints
  default to unconfirmed and cannot load until re-added with the required one-controller checkbox.
  The public output boundary rejects an unconfirmed load before Buffering, epoch advancement, or
  worker enqueue and marks the exact UI generation retryable; the worker owns an independent final
  gate so every load intent—including a pre-dispatch media rejection—cannot clean up, connect,
  issue MPD state/options, register a protected ticket, or mutate the queue even if a caller
  bypasses GTK. Confirmation
  authorizes the unavoidable global operations but does not weaken foreign-song defenses: an
  observed foreign ID proves the deployment promise was violated, and Tributary still relinquishes
  ownership without a racy stop or deletion.

- 2026-07-17 — P3.5 uses one exactly pinned Linux x86_64 aggregate as the comparable coverage
  threshold. Splitting unit and integration runs would fragment one host's denominator without
  solving the actual platform mismatch, so every Linux host target and feature contributes to one
  report. Every developer helper also stops excluding source areas but remains informational: it
  uses the active compiler, and conditional compilation gives native platforms different
  denominators. CI enforces only the value committed on the branch; the non-decrease ratchet is
  repository review policy. Lowering the floor requires a dedicated measurement-definition change
  that explains and remeasures a tool or source-set transition.

- 2026-07-17 — P3.2 treats local album/artist identity as a versioned deterministic projection of
  exact stored metadata, not a new persisted schema. The former UUIDs were random per listing and
  therefore supplied no durable aggregate references to migrate; a private v1 UUIDv5 namespace,
  separate artist/album domains, component-count framing, and big-endian length framing now define
  the contract and pinned goldens. Performing-artist name is the artist key. Album title plus
  effective album artist is the album key: only absent or Unicode-whitespace-only album-artist
  metadata falls back to the performing artist, while every nonblank byte sequence is retained.
  Year is deliberately excluded because missing/inconsistent tags and ordinary year edits must not
  split aggregate identity; year and genre are display aggregates with deterministic minima.
  `Album.artist_id` stays absent because a compilation album artist may have no corresponding
  performing-artist row. A metadata edit to an identity component intentionally creates a new
  aggregate rather than guessing continuity. UUIDs are not reversible, so by-ID methods first scan
  compact grouped keys and then load only rows under the resolved exact title or artist; album
  results reuse the same effective-artist predicate. At PR #114 this decision closed stable
  aggregate and lookup behavior only; it did not yet make `LocalBackend` the shipping integration
  seam or change the invalid database track-UUID fallback. PR #116 subsequently closed the
  complete-catalogue seam, and PR #120 preserved exact persisted `TrackId` strings, removed the
  random fallback, and added exact local/playlist ID-at-use resolution. At that milestone,
  current-root authority, containment, retention through output consumption, and broader source
  lifecycle integration remained P3.1 work. PR #121 closed playback/output retention, and the
  current follow-up closes retained local/playlist embedded-art parsing; non-remote registry
  adapters remain.

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
  event storm. The first implementation eagerly retargeted an already-captured playback queue;
  the later P3.1 local-ID slice supersedes that compatibility path by keeping queue items pathless
  and resolving the exact database ID on every load. Already-open playlist rows are still
  retargeted by the same stable ID for display and file-management actions, and an
  in-flight playlist result overlays committed local URIs before it can render. Same-key request
  generations and post-reconciliation reloads remain P1.9. A rename that falls back to
  reconciliation still mints new track IDs, so the ID-based refresh cannot repair a queue captured
  before it; recovery requires rebuilding it from a refreshed source model. Exact-ID-at-use
  resolution supersedes only the eager path refresh; a retained authority lease through playback
  and receiver consumption remains P3.1.
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
  `tributary-remote://<lease>/{stream,artwork}/id-<lowercase-hex-native-id>`; the fixed reversible
  prefix prevents `.` and `..` from normalizing as path segments. Same-source replacement, release,
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
  and playback-time remote-resolution boxes. The later P3.1 identity milestone supplies stable
  source IDs, exact local/remote IDs, playlist/radio view identity, and removable/external
  identities; PR #121 then closes exact local/playlist ID-at-use and retained output authority.
  The authenticated-remote production cutover subsequently replaces the original standard lease
  URI and sibling DAAP reference with pathless source/track/epoch queue state, one registry, and
  centralized refresh/failure/provenance projection. The retained embedded-art follow-up then
  removes the final local/playlist path-based consumer. P3.1 still needs at-use Radio-Browser,
  removable, and external-file locator adapters.
- 2026-07-15 — A late PR #86 review found that Plex catalogue publication did not distinguish a
  track with no `Media`/`Part.key` from a resolvable track: GTK received a non-empty opaque
  reference that failed only after selection, whereas the old direct-URL path had left that row
  disabled. The follow-up omits tracks with no non-empty part and searches all media/part entries
  for a later usable locator, binding bitrate/format to the same selected media entry and
  preserving the resolver invariant that every published Plex track has a backend-private stream
  locator.
- 2026-07-12 — P1.7 places the non-`Send` Cast transport, application, media session, controls,
  heartbeat, status polling, and teardown on one FIFO worker. Epoch checks bracket every Cast
  effect and event; ownership is recorded immediately after application launch and media load so
  superseded calls remain cleanable. Failures retire the session before publishing Stopped then
  Error, clean media/application ownership with fair bounded retries, and abandon an unreachable
  application after three attempts so a replacement load can reconnect. The legacy local-file
  resolver remains synchronous; P1.6's upstream ticket proxy does not change that local-path
  lookup behavior.
  `rust_cast 0.21`'s high-level `CastDevice` uses blocking `TcpStream` calls and hides the socket,
  so P1.7 could check cancellation only before and after an in-flight call. Hard receiver I/O
  deadlines were therefore tracked as P2.8 rather than overstated as part of P1.7; P2.8 later
  closed the gap through the crate's public generic lower-level channel APIs.
- 2026-07-16 — P2.8 uses the mDNS-advertised numeric IPv4 endpoint because the existing
  Chromecast media-ticket listener is IPv4-only; accepting an IPv6-only control endpoint would
  advertise media the receiver cannot fetch. The selected control endpoint rejects non-routable
  values and address changes retire the old publication before a replacement appears. One
  absolute deadline spans each high-level channel operation and every underlying TLS read/write,
  while a shorter idle cap detects silence promptly. The `Rc` transport stays on its original
  worker rather than crossing threads. A complete Cast rejection leaves framing synchronized and
  permits bounded cleanup; any I/O, TLS, framing, parsing, decoding, or timeout failure drops the
  connection immediately, including after supersession. P3.4's 2026-07-17 framing slice later
  closed the separately tracked peer-sized allocation boundary with a 1 MiB pre-allocation cap
  above rustls and four regressions through the real `rust_cast` message manager.
- 2026-07-13 — P1.8 gives MPD one FIFO worker and one persistent TCP session per owned load.
  Stable `addid`/`playid` identity, authoritative status polling, and targeted `deleteid` cleanup
  distinguish explicit Stop and remote errors and detect an observed replacement queue. Loads
  never clear the shared queue, and controls revalidate the current ID before acting. Every owned
  load explicitly disables MPD `repeat`, `random`, `single`, and `consume` modes so queue
  exhaustion remains attributable to Tributary. Protocol lines, responses, resolved-address
  counts, media-URI sizes, idle I/O, and post-resolution operations are bounded; poisoned streams
  are dropped rather than reused, and all diagnostics discard server text and authenticated URLs.
  P2.10's PR #104 follow-up makes the remaining terminal proof explicit: after an
  owned item was active, stopped/no-current plus successful targeted deletion is completion,
  whether caused by natural exhaustion or an indistinguishable external Next past the final item.
  `NoExist` instead proves only that the ID was already absent and produces ownership loss without
  EOS, while every other correlated ACK remains a cleanup failure. ACK framing, index zero, and the
  expected command echo are validated before only the closed numeric code is retained; malformed
  or mismatched ACK-like input poisons the connection. MPD's index, echo, and free-form text are
  then discarded.
  P2.10's final slice makes shared/unconfirmed playback unsupported: the persisted mode defaults
  false for legacy entries. The public boundary refuses loads before Buffering, epoch advancement,
  enqueue, or cleanup and retains a retryable UI generation; a defense-in-depth worker gate rejects
  every load intent before an MPD connection, state or option command, or protected ticket,
  including media failures classified before dispatch. Re-adding the exact endpoint with the
  required localized checkbox upgrades it in place. If a foreign current song is later observed
  in confirmed exclusive mode, Tributary deliberately relinquishes the session without deleting
  its queued ID, including during explicit Stop and shutdown. Revalidation plus `deleteid` is not
  atomic, so a foreign client could select that ID between the two operations. A stale response
  that reports
  foreign ownership drops the old session before replacement cleanup can target the retained ID.
  Protected tickets are revoked and no retained entry contains a backend credential, but a direct
  entry may remain playable and a protected entry may remain selectable until its revoked route
  fails. PR #105's bounded command ingress preserves exact FIFO
  below 64 pending commands, atomically replaces older-epoch backlog, and only under saturation
  folds adjacent same-epoch transient controls before deterministically evicting the oldest
  remaining transient. A capacity-one wake signal never carries commands and stale wake tokens
  cannot renew the worker's
  polling deadline. PR #106 replaces blocking standard-library name resolution with one
  process-lifetime resolver thread and private GLib main context. Its capacity-64, 1-KiB-host
  ingress admits at most 16 requests per context tick and eight active GIO enumerations, failing
  overload closed while dispatching callbacks between intake batches. The same absolute operation
  deadline covers resolution, connection, and greeting; deadline, lifecycle supersession, and
  result-channel loss cancel GIO, and late callback sends and drops remain on the resolver thread.
  Numeric addresses bypass DNS, while enumerated IPv4/IPv6 results retain order, deduplicate to 32,
  and preserve IPv6 flowinfo and scope. PR #107 proves the per-address greeting deadline is fair to
  a later resolved address after an accepted first peer stays silent, and exercises the complete
  numeric `::1` resolution/connect/greeting path against a real listener when IPv6 loopback is
  available. MPD still has no ID-scoped pause or conditional compare-and-act; PR #112 therefore
  requires a detectable persisted exclusive-control configuration and fails every
  unconfirmed load closed rather than overstating protocol isolation as part of P1.8.
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
- 2026-07-16 — P2.11 separates deterministic HTTP-boundary compatibility from the remaining
  packaged GStreamer proof. A configured base path is an opaque reverse-proxy prefix: appending
  removes only its optional trailing empty segment and never clears, decodes, or re-pushes the
  existing segments, because doing so would either discard the prefix or double-encode escaped
  bytes. DAAP media needs the same four non-secret protocol headers as the control session, but
  those values are neither credentials nor receiver input. `ResolvedHttpRequest` therefore keeps
  them in a dedicated exact-name allowlist, disjoint from authentication headers and private query
  pairs. Only `Accept`, `User-Agent`, `Client-DAAP-Version`, and
  `Client-DAAP-Access-Index` may cross that channel; routing, range, cookie, referer,
  authentication, proxy, framing, hop-by-hop, and arbitrary names fail closed. The app-owned
  stream and artwork fetches install trusted protocol state before authentication, while a
  playback receiver can still contribute only `Range`. Explicit upstream proxy selection remains
  supported and is now exercised at the actual asynchronous protected-fetch boundary. This
  deterministic slice can close independently. At that point the fake full-pipeline, packaged
  source-plugin, and live Windows playback proofs remained one compound task; the 2026-07-17
  decision below splits and closes the fake proof while retaining packaged source-plugin and live
  Windows evidence as the open task.
- 2026-07-17 — P2.11 treats a process-isolated run through the real production `Player` as the
  complete fake-backend pipeline proof. DAAP- and Subsonic-shaped requests therefore exercise the
  same typed request builders, per-load app proxy, `souphttpsrc` policy, decoder, player bus watch,
  generation ownership, and EOS handling as local playback, with only the physical audio sink
  replaced by `fakesink`. Hard plugin requirements and parent/child deadlines turn missing or hung
  native support into a failure rather than a skip. This closes an automatable pipeline task, not
  the packaged-Windows task: Cargo tests use the build host's plugin registry and DLL/shared-library
  set, so they cannot prove the bundled artifact selects the same source, carries every required
  runtime file, or works with either reported live server.
- 2026-07-17 — P2.11 separates deterministic packaged-runtime proof from live server validation.
  The Windows distribution tree is the artifact boundary: before ZIP creation, its own executable
  must initialize with a fresh external registry, no ambient plugin/system path or proxy resolver,
  and only DLL search roots that a user receives. A successful probe requires dynamic factory and
  decoder provenance beneath that tree, a decoded FLAC buffer and EOS through the production
  protected-ticket callback, no poisoned-proxy connection, and a fixed fail-closed result for an
  alternate source. Bundling `gst-plugin-scanner.exe` is part of that contract because allowing a
  system helper would make the artifact appear complete only on the build host. The helper is kept
  beside the packaged app and root-level DLL set, and the probe's `PATH` contains only System32, so
  scanner loading has the same executable-directory dependency search available during an ordinary
  user launch. Dependency collection is a bounded, deduplicated queue over the app, scanner, all
  copied plugins, and every newly copied MSYS2 runtime. The closure reads PE import tables with the
  exact architecture's absolute `llvm-readobj.exe` instead of executing plugins through `ldd`,
  inspects Soup alone for direct-edge attribution, and batches the remaining targets beneath
  command-line, output, process, process-tree, and whole-closure limits. The exact helper is executed under a five-second deadline before GStreamer
  initialization so a missing dependent DLL or wrong-architecture scanner cannot be hidden by
  in-process plugin discovery. The probe is
  process-isolated, deadline-bounded, and sentinel-backed so a crash, skip, output flood, native
  hang, or zero-work exit cannot pass. Its listener deadlines arm only after cold plugin discovery,
  while the enclosing process deadline covers that startup; malformed or unwritable media requests
  are fatal rather than ignored. This automated result is independently closeable, while
  `.local` routing, real DAAP/Subsonic behavior, physical output, firewall/endpoint security, and
  end-user proxy policy remain one explicit live packaged-Windows task.
- 2026-07-17 — The P2.11 packaged listener distinguishes narrow teardown cancellation from request
  failure. A PR #112 ARM64 package attempt completed the same 498-target dependency closure and then
  reported only the probe's fixed loopback-server failure; a same-source main run passed the
  unchanged packaged probe. That evidence shows a transient result but cannot prove its root cause.
  PR #114 therefore hardens one plausible race without retrying or broadening success:
  production publishes cancellation before moving GStreamer to NULL but keeps both listeners live;
  final stop is separate and drains/counts queued accepts before join. Only an already-accepted
  incomplete-header EOF, connection-aborted, or connection-reset result may cancel once that phase
  is visible. Semantic request drift, timeouts, other I/O, every accept failure, and response-write
  failure remain fatal regardless of teardown, while poison observation covers the entire NULL
  transition. Deterministic listener regressions preserve both sides and the observation window;
  P2.11 checklist arithmetic is unchanged.
- 2026-07-17 — P2.11 resolves the completed Windows distribution through PowerShell's filesystem
  provider before using `.NET` path APIs. PowerShell's provider-relative commands follow `$PWD`,
  but `Environment.CurrentDirectory` remains at the process launch directory after `Set-Location`;
  therefore `Test-Path dist\...\libgstsoup.dll` could succeed immediately before
  `Path.GetFullPath` named a different, nonexistent tree. Retaining the caller-relative dist name
  preserves local and CI output layout, while canonicalizing it immediately after creation gives
  the Soup singleton, every copied plugin, the scanner, and all recursively copied runtimes one
  absolute distribution root. `Resolve-Path -LiteralPath` is Windows PowerShell 5.1-compatible;
  selecting its physical `ProviderPath` treats spaces as path data and prevents a custom FileSystem
  PSDrive name from reaching `.NET` or an external executable. This fixes entry into the
  already-required packaged probe and does not substitute for the open live DAAP/Subsonic playback
  evidence.
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
| 2026-07-16 | P2.7 platform runtime caches | PR #101 | Moved bundled GStreamer registries to install-keyed per-user caches before toolkit initialization, removed executable-adjacent fallback, added record-structured validation and exact absolute-path rewriting for the signed macOS pixbuf helper's generated cache, and made a path-with-spaces/read-only post-sign runtime probe plus final strict signature verification part of macOS packaging. Sixteen focused policy/static tests cover the portable contract, the independent fuzz lock remains synchronized, and PR CI passed the native macOS relocation/signature probe plus both Windows architecture jobs. |
| 2026-07-16 | P2.8 bounded Chromecast control | PR #102 | Rebuilt the public `rust_cast` channel graph over a worker-local deadline-aware TLS stream, retained deterministic usable numeric mDNS IPv4 endpoints, and bounded TCP connect, operation-wide, and idle read/write time without moving the `Rc` session across threads. Poisoned sessions are dropped immediately while synchronized semantic rejections retain bounded cleanup; 12 focused real-socket, trickle, lifecycle, classifier, supersession, and discovery regressions cover the contract. |
| 2026-07-16 | P2.10 ACK/terminal/orphan semantics (partial) | PR #104 | Retains only correlated typed redacted MPD ACK codes and accepts only `NoExist` as proof that a targeted ID is already absent; malformed or mismatched ACK-like input poisons the connection. Successful targeted removal supplies the stopped/no-current completion proof shared by natural exhaustion and external Next, while an already-absent ID is ownership loss. After a foreign replacement is observed, polling, explicit Stop, and shutdown deliberately retain the queued ID because shared-mode revalidation cannot make deletion conditional; a stale foreign-status response also drops its old session before replacement Load can target that retained ID. Protected tickets are revoked, so a retained entry carries no backend credential. Seven new focused parser, real-socket, cleanup, terminal, foreign-owner, and stale-supersession regressions cover the slice; PR #105 through PR #107 follow with bounded ingress, cancellable resolution, and deeper socket coverage. Only exclusive-control/global-option semantics remain open. |
| 2026-07-16 | P2.10 bounded MPD ingress (partial) | PR #105 | Replaces the unbounded worker channel with a capacity-64 epoch-aware deque plus a coalesced capacity-one wake signal. Below the cap commands remain exact FIFO; a newer lifecycle epoch atomically purges stale backlog, and only saturation folds adjacent same-epoch transient controls before deterministic oldest-transient eviction keeps the newest intent. Wake handling retains one absolute polling deadline, receiver loss clears pending work, and no enqueue waits on network I/O. Six regressions include a real held-ACK peer proving prompt enqueue, no command pipelining, and ordered Seek/Play execution after release. PR #106 and PR #107 follow with cancellable resolution and slow-greeting/real-IPv6 coverage; only exclusive-control/global-option semantics remain open. |
| 2026-07-16 | P2.10 cancellable MPD resolution (partial) | PR #106 | Replaces blocking `ToSocketAddrs` with a process-lifetime private-context GIO resolver. Capacity-64, 1-KiB-host ingress processes at most 16 requests per context tick and caps active operations at eight; overload fails closed and callbacks run between batches. One absolute load/probe deadline spans resolution, connection, and greeting, while lifecycle supersession or result loss cancels GIO and late callback sends/drops remain on the resolver thread. Numeric addresses bypass DNS; enumerated results preserve order and IPv6 flowinfo/scope while deduplicating to 32. Nine net-new regressions plus one expanded raw/bracketed IPv6 case cover the contract. PR #107 follows with slow-greeting/real-IPv6 socket coverage; only exclusive-control/global-option semantics remain open. |
| 2026-07-16 | P2.10 MPD real-socket coverage (partial) | PR #107 | Completes the compound socket-coverage item begun by PR #105's held-ACK FIFO peer. A channel-held silent first IPv4 greeting proves the shared absolute deadline preserves a fair slice for a later address without sleeps or elapsed-time thresholds. A real `::1` listener proves numeric resolution, connection, greeting, and IPv6 client/peer addresses; only an unavailable initial IPv6 bind skips that capability-specific case. Two net-new regressions bring the focused MPD suite to 79. Only exclusive-control/global-option semantics remain open. |
| 2026-07-17 | P2.10 MPD exclusive-control contract | PR #112 | Persists an explicit legacy-default-false mode; requires localized partition-wide warning and one-controller confirmation; upgrades an exact endpoint in place; makes mode part of output identity; refuses unconfirmed public loads before Buffering/epoch/enqueue while retaining retry state; and independently gates every load intent—including pre-dispatch rejection—in the worker before cleanup, MPD connection/state/options, protected tickets, or queue mutation. Foreign-ID relinquishment remains fail-safe even when the confirmed deployment promise is violated. Nine focused migration, upsert, identity, boundary/worker zero-action, retry, and localization tests cover the final slice. Two actionable Codex findings were fixed and resolved; the final review and full native/package CI matrix passed. |
| 2026-07-17 | P3.3 service fixtures and DAAP adversarial/session harness (partial) | `b80e534`, `6f6c9ac` | Adds one bounded keyed HTTP fixture for five non-DAAP production success paths and integrates DAAP's protocol-appropriate deadline/header-capped, fragmentation-forcing, endpoint-scripted raw-socket fixture with owned handlers. DAAP rejects nine wrong, truncated, over-deep, invalid-width, duplicate-status, or valid-prefix/malformed-remainder catalogue forms without registry publication or media access; shares HTTP 401/403 expiration handling across update/databases/items; classifies in-band 401/403 and non-auth `mstt`; and proves fixed diagnostics plus one automatic bounded logout after every post-login failure. Independent reviews found no actionable issues. This closes the named all-services mock and DAAP malformed/session-expiration boxes; the cross-service adversarial behavior matrix and final P3.3 implementation record remain open. |
| 2026-07-17 | P3.3 cross-service behavior matrix (completion) | PR #118 | Adds bounded delayed-body behavior to the keyed fixture and a documented representative matrix for rejected authentication, authenticated/public redirects, deadline, streaming body cap, Jellyfin/Plex multi-page catalogues, Subsonic/Plex/geolocation partial failure, and all four authenticated protocols' reverse-proxy paths. The matrix strips server-controlled Subsonic failure text, fixes Radio-Browser/geolocation acceptance of non-success JSON, repairs trailing-slash Jellyfin/Plex API paths, and prevents Plex stream/artwork prefix erasure and dot-segment escape. Review additionally pinned root `//` and prefixed `/share//` to the same one-empty-segment rule across API and protected-media construction. Twelve new tests bring the completion branch to 787 tests per profile with strict debug/release Clippy, formatting, and whitespace checks. This closes P3.3's final behavior and implementation-record boxes. |
| 2026-07-15 | P2.11 protected-playback urgent slice | PR #96 | Shared pooled upstream transport with independent connect/header/body-idle budgets; validated direct-only local and AirPlay ticket sources; localized fixed-category, secret-free proxy/GStreamer/backend diagnostics; one-shot terminal handling; and 13 focused regressions including an isolated poisoned-proxy process plus catalog-wide translation checks. Retained mDNS routing and packaged full-backend Windows playback remain open. |
| 2026-07-15 | P2.11 retained mDNS address routing | PR #97 | Exact service-instance ownership, bounded origin-indexed duplicate aggregation, bounded ephemeral exact-origin routes through applicable API/auth clients and protected stream/artwork pools, unchanged hostname/Host/TLS/proxy behavior, pre-network loss invalidation, and DAAP bearer isolation in revocable typed requests. Thirty new focused regressions plus strengthened DAAP-lifecycle and cast-proxy integration coverage exercise route canonicalization, IPv6 scope, discovery update/removal/alias/cap semantics, stalled resolvers, explicit-proxy preservation, backend propagation, auth-attempt ownership, end-to-end Host/auth/ticket containment, and ephemeral UI identity. Full packaged-Windows/backend playback validation remains open. |
| 2026-07-16 | P2.11 deterministic HTTP compatibility (partial) | PR #108 | Preserves exact escaped reverse-proxy prefixes across DAAP stream/artwork and Subsonic API/media construction, carries DAAP's four fixed protocol headers through a separate strict non-secret allowlist into protected stream and artwork fetches, retains receiver `Range` as the only forwarded header, proves existing typed Subsonic HTTP-200 failures, and exercises explicit upstream proxy selection at the asynchronous protected-fetch boundary. Seven net-new regressions cover the contracts. At PR #108, full fake GStreamer, packaged source-policy, and live Windows playback validation remained open; the following slice closes the fake-GStreamer part. |
| 2026-07-17 | P2.11 real-GStreamer fake-backend path (partial) | PR #109 | Process-isolated DAAP- and Subsonic-shaped typed requests traverse the production Player, protected loopback proxy, HTTP source, FLAC decoder, and fakesink to generation-owned EOS while preserving exact upstream request and direct-source-policy contracts. Packaged Windows source-policy and live playback remain open. |
| 2026-07-17 | P2.11 packaged Windows runtime proof (partial) | PR #110 | The completed Windows distribution computes a bounded, non-executing PE-import closure over the app/scanner/all plugins and each copied runtime, with a singleton Soup direct-edge gate and batched absolute architecture-local `llvm-readobj` processes; this replaces an ARM64 `ldd` hang while retaining exact recursive copying and no broad runtime sweep. It co-locates and directly preflights its exact scanner without probe-only DLL search help, then runs its own hidden early-startup probe with sanitized runtime/proxy state and a fresh external registry before ZIP creation. Native x86_64 and ARM64 CI both prove bundle-only factory/decoder provenance, real protected-ticket FLAC decode/EOS, exact direct/zero-retry/30-second source policy, zero poisoned-proxy connections, and alternate-source fail-closed behavior under Rust and process-level deadlines. Live packaged DAAP/Subsonic playback remains open. |
| 2026-07-17 | P3.1 source identity/lifecycle architecture | PR #113 | Records location-independent `(SourceId, TrackId)` media identity, an exact legacy-array-to-versioned-envelope saved-source migration with atomic replacement and fail-closed conflict quarantine, one registry-owned operation/session lifecycle, deterministic per-view radio locator ownership, playback-time locator resolution, exactly-once DAAP retirement, adapter-specific rules for every source kind, and staged completion tests. This closes only the two architecture decision boxes; runtime migration remains open. Independent review found and resolved two design ambiguities before the full exact-toolchain/native/package matrix passed in runs 29605029668 and 29605032344. |
| 2026-07-17 | P3.2 stable local aggregates (partial) | PR #114 | Local tracks, artist listings, and album listings share private versioned, domain-separated, length-framed UUIDv5 identities over exact metadata. Album grouping, counts, and stats use exact title plus effective album artist with Unicode-blank fallback and deterministic metadata minima. Both formerly unsupported by-ID methods resolve compact keys, narrow SQL to exact title/artist, reuse the grouping predicate, order deterministically, and return empty for unknown IDs. Four focused backend tests, all 243 local tests, the 759-test debug repository suite, static analysis, and the implementation-head exact-toolchain/native/package matrix passed. A documentation rerun exposed and fixed a pre-existing terminal MPD enqueue/wake race; replacement static run 29614521885 and complete matrix 29614525132 passed every job. At PR #114 the common `LocalBackend` catalogue seam, final P3.2 record, and invalid persisted `TrackId` repair remained open. PR #116 closed the first two; PR #120 later preserved the exact persisted string and removed the random fallback under P3.1. |
| 2026-07-17 | P2.11 packaged-probe teardown hardening (follow-up) | PR #114 | Separates pre-NULL cancellation from final listener stop, keeps poison observation live through NULL, and drains/counts queued accepts before join. Only narrowly classified incomplete-header EOF/reset/abort outcomes may cancel; semantic request, accept, and response-write failures remain fatal even during teardown. Four Windows tests pin classification, synchronized clean-versus-malformed behavior, and the accepted/queued poison window. x86_64 runs the unit tests, while ARM64 compiles the code and executes the production probe during packaging. This hardens the already-complete packaged proof and leaves checklist arithmetic unchanged. |
| 2026-07-17 | P2.11 Windows distribution-path repair (follow-up) | PR #115 | Resolves the caller-relative bundle immediately after creation through PowerShell's FileSystem provider and retains its physical `ProviderPath`, preventing `.NET` PE inspection from using a stale process working directory and preventing custom PSDrive names from escaping into native tools. Static ordering and live PowerShell regressions cover a changed `$PWD`, repository spaces, and custom FileSystem PSDrives. Full debug/release suites pass 761 tests per profile, all 30 focused platform-runtime tests pass in both profiles, and strict Clippy is clean. CI run 29615869107 passed every job, including both native finished bundles; the live packaged DAAP/Subsonic playback box remains open, so checklist arithmetic is unchanged. |
| 2026-07-17 | P3.2 shared catalogue backend completion | PR #116 | Adds object-safe complete-track catalogue access and removes every backend-specific `all_tracks` bypass. Scanner snapshots construct `LocalBackend`; local, environment, manual, discovery, Subsonic, Jellyfin, Plex, and DAAP publication all enter one explicit `&dyn MediaBackend` adapter. Concrete authentication and protected-media/session retention intentionally remain under P3.1. The production passwordless DAAP catalogue-error branch logs out and invokes the paired user-error/GTK-cleanup helper; focused coverage pins that helper's paired emissions without claiming to synthesize a catalogue failure. Together with PR #114's aggregate work, this closes P3.2's final two boxes. Its base-branch validation is recorded above. |
| 2026-07-17 | P3.4 keyboard/context-menu and slider accessibility (partial) | PR #119 | Closes the accessibility item within the combined P3.4 completion. Unmodified Menu and exact Shift+F10 share right-click's immutable selection snapshot and popover-owned actions, propagate when no menu can open, and publish the standardized popup shortcut. Position and volume scales retain GTK's native slider semantics while gaining distinct localized names in all 13 catalogs. Two focused regressions pin the production-consumed interaction/accessibility plans and parse every source YAML catalog. |
| 2026-07-17 | P3.4 MPD/Chromecast integration harnesses (partial) | PR #119 | Closes the compound receiver-state-machine item within the combined P3.4 completion. The established 83-test fake/real-socket MPD harness and 38-test delayed/adversarial Chromecast harness are joined by a plaintext 1 MiB Cast frame guard immediately below the real `rust_cast` manager. Four new real-manager regressions prove pre-allocation oversized rejection without payload reads, exact-limit acceptance, truncated-header/payload failure, and consecutive-frame reset/write preservation, bringing the focused Chromecast module to 42 tests. |
| 2026-07-17 | P3.4 recycled-row and UI-generation harnesses (partial) | PR #119 | Closes the GTK recycling/stale-callback and stale artwork/source-result boxes within the combined P3.4 completion. Sidebar setup installs one parameterized production `GAction`; factory bind replaces its typed current-source target and unbind revokes it. Its headless repeated-recycle harness proves no historical delete/disconnect/menu callback survives. A synchronized loopback fixture drives the real persistent artwork worker across an in-flight generation replacement, and the production source cache/eviction boundary rejects reversed same-key loaded and missing callbacks while retaining cache-only publication for an inactive newest request. |
| 2026-07-17 | P3.4 playback/output lifecycle completion | PR #119 | Completes the playback-session, output-transfer/reselect, and final implementation-record boxes. Production now shares exact `ListModel` queue capture, current-item direct load/retry, and clear-before-Stop ownership boundaries; the headless harness survives sort/filter/source-projection replacement while preserving local B→C→B identity and rejecting stale events. A validated output-slot transaction makes exact reselection inert, parks/restores only an exact Local output, clears before stopping/dropping remote outputs, preserves playback on invalid or wrong-parked-type replacement, and removes the throwaway MPD parking worker. Recording Local/MPD/Chromecast outputs prove rejection retry, remote replacement, exact Local restoration, and complete cleanup. The final rebased branch passes 797 tests per profile and strict Clippy in both profiles. |
| 2026-07-17 | P3.1 exact local ID-at-use resolution (partial) | PR #120 (`81db425`) | Preserves each SQLite `tracks.id` byte-for-byte in local and playlist UI/queue identity, replaces random malformed-UUID fallback with a frozen deterministic compatibility projection, removes file locators from local/playlist queue items, and resolves the exact current database row plus five-second regular-file evidence before initial, newly navigated, repeated, or receiver-targeted output loads. Exact-generation completion suppresses stale async results and missing, empty, dead, timed-out, or deleted IDs never use metadata/path fallback. Focused resolver, queue, GObject, and model-conversion tests plus strict full debug/release suites cover the slice. Acquisition of the current root, containment proof, exact file authority, and retention through full output consumption remain open, so that compound task remains unchecked. |
| 2026-07-17 | P3.1 stable identity runtime | PR #120 (`79b9d0c`, `9bf87db`, `8232d90`, `432b66b`, `269eb93`, `4bb27a3`, `208dbf4`, `3357765`, `3adf3ee`) | Freezes typed source/media/view identity and a strict atomic saved-source migration; replaces URL ownership with persisted or deterministic `SourceId` across standard and DAAP flows; preserves exact bounded native IDs for every remote adapter; and adopts source-scoped `MediaKey` plus playlist/radio `ViewOrigin` in playback. Version-1 rows accept only random RFC UUIDv4 IDs or their exact canonical remote UUIDv5 owner, preventing crafted values from claiming another endpoint, backend, built-in, or removable identity. Repeated Add reuses the saved owner, discovered-to-saved promotion persists the already-published ID and retains its ephemeral advertised route for the immediate route-aware connection, and saved-plus-env startup authenticates under the stored ID; a prefixed reversible segment round-trips even native `.`/`..` IDs through stream and artwork references. Accepted reconnect publications clear the transient connecting state even when the canonical owner remains connected; environment authentication/catalogue failures carry exact owner plus opaque UI-attempt identity, preserving a retained predecessor and rejecting a stale queued failure after a same-source retry. Removable rows use frozen lossless mount-relative native identity, malformed radio IDs fail closed, and external sessions mint independent random IDs. A row carrying both saved and discovery provenance still collapses those facts into `manually_added`, so Delete cannot yet demote it to a still-live discovered row and discovery loss can still retire the saved owner; that remains part of the open centralized-lifecycle task. Focused migration, route, registry, exact-native-ID adapter, queue, removable, radio, source-object, and reordered exact-attempt failure regressions cover the completed stable-ID compound box; the final independent-review head passes 827 tests and strict Clippy in both debug and release. Centralized lifecycle/provenance, nonlocal at-use locators, and acquisition/containment/retention of local root/file authority remain open. |
| 2026-07-17 | P3.1 stable identity CI follow-up | PR #120 | Replaces the failed-atomic-migration fixture's Unix permission manipulation with an injected error at the loader's private save boundary. Privileged containers can bypass `chmod 0500`, but the new cross-platform fixture deterministically proves a failed replacement quarantines the configuration, publishes no migrated rows, and preserves the legacy bytes exactly. The focused regression passes in debug and release; it now also runs on Windows, changes no production migration behavior, and leaves checklist arithmetic unchanged. |
| 2026-07-17 | P3.1 exact local/playlist ID-at-use and retained output authority | PR #121 | Preserves PR #120's exact SQLite IDs and typed local `MediaKey`/playlist `ViewOrigin`, keeps playback queues pathless, and resolves only the exact current row beneath the most-specific currently configured authoritative root. A typed `ResolvedLocalMedia` lease retains root, marker, ancestor, and exact file handles through local/AirPlay GStreamer, Chromecast, and MPD handle-backed tickets; path replacement cannot retarget admitted playback, explicit-offset full/Range reads cannot interfere through a shared cloned-handle cursor, and load replacement, Stop, error, terminal completion, ticket drop, and teardown revoke future lookup. Shared Chromecast cleanup revokes credential and retained-authority routes without changing the legacy explicit-file server-lifetime contract. Gemini's review follow-up replaced whole-row root equality with a semantic snapshot that ignores observational timestamp drift while binding every root-authority field. All 56 focused root/state recheck, no-fallback, symlink-escape, retained-file replacement, ticket/revocation, output-boundary, and playlist-ownership regressions pass alongside locked all-target check, formatting/diff checks, strict debug/release Clippy, and complete 834-test debug/release suites. Central lifecycle/provenance, nonlocal at-use locator adapters, and the path-based local embedded-art helper remain open. |
| 2026-07-17 | P3.1 centralized lifecycle foundation (partial) | PR #122 | Adds an intentionally unwired `SourceLifecycleRegistry` with atomic adapter/production-lease/epoch ownership, framework-only construction and unforgeable close authority, phase-safe protected construction plus staged retirement, exact connect/refresh generations, correlated sanitized failures, keyed/refcounted provenance, exact reusable disconnect waiters, post-resolution lease/epoch rechecks, atomic task admission, a persistent shutdown barrier, and final-handle fail-closed teardown. Thirty-eight deterministic adversarial tests cover pre/post-stage cancellation and panic (including synchronous closure invocation), supersession ordering, stale media/refresh rejection, provenance reappearance, reconnect/disconnect close races and waiter/event/barrier finalization, late task admission, replacement, views, shutdown, and pruning. A Windows x86_64 CI interleaving exposed that waiter publication preceded release of the retirement task's barrier participant; the follow-up now finalizes state/events, releases barrier participation, and only then wakes the waiter, so waiter completion and shutdown completion agree. Standard/DAAP/GTK production owners remain unchanged, the DAAP login constructor still needs its required post-session-ID/pre-update split, and the centralization and final implementation boxes remain open. |
| 2026-07-18 | P3.1 authenticated-remote lifecycle production cutover (partial) | PR #123 | Makes `RemoteSourceRegistry` the sole production owner for Subsonic, Jellyfin, Plex, and DAAP connection, catalogue, media epoch, failure, disconnect, and shutdown state; removes the sibling standard/DAAP registries; and makes GTK consume one atomic baseline/watch reducer. Exact accepted catalogues clear their pending guard before rebind and reactivate an already-selected row only after publication is authoritative; stale generations remain inactive, and programmatic selection releases `RefCell` guards before synchronous GTK re-entry. Independent Saved/Environment/Discovery claims drive demotion and visibility; discovery withdrawal clears its advertised route and revokes route-bound active/pending work even when another claim preserves the row. Per-generation connect settlements let one composite disconnect waiter join superseded construction, late rejected-adapter close, an adopted adapter, and a dissociated predecessor, propagate a sanitized late close failure, and let final-claim release retire/prune without duplicate close. DAAP stages exact close authority after `mlid` and before update/database/items. Interactive Jellyfin stages its owned token before ping/catalogue, cleans up a safely representable token if final client construction fails, never logs out a pre-existing API key, and fails closed without unsafe logout for the narrow control-byte-header case. Menu and Ctrl+Q quit requests enter the active window's shutdown barrier. The focused lifecycle module passes 53 tests; locked debug and release suites each pass 20 library, 865 application, and 10 repository-metadata tests (895 total), with locked check, strict Clippy, formatting, and diff checks green. Radio/removable/external at-use adapters and local embedded-art authority keep the final implementation box open at 219/223 overall and 29/30 P3. |
| 2026-07-18 | P2.11 Windows PE-import argument-binding repair (follow-up) | PR #124 | Replaces positional calls to the non-terminal `[string[]] Paths` parameter with explicit names for every singleton and batch inspector argument, removing the suspected version-dependent route by which a deadline or limit could be treated as a PE target. The regression checks the declaration and each of the exactly two production calls independently. PowerShell 7 parses the complete script, all 11 focused Windows contracts pass, and locked debug/release suites each pass 20 library, 866 application, and 10 repository-metadata tests (896 total) with locked check and strict Clippy green. CI run `29633729566` passed every job, including production bundle/probe execution on native x86_64 and ARM64 Windows runners. Import targets, limits, and fail-closed validation are unchanged. The exact affected Windows PowerShell/MSYS2 build and the separate live DAAP/Subsonic playback proof remain pending, so checklist arithmetic stays at 219/223 overall and 76/79 P2. |
| 2026-07-18 | P3.1 retained local/playlist embedded-art authority (partial) | PR #125 | Starts embedded-art work only after exact local resolution remains current and the selected output accepts its retained load, then gives a cloned `ResolvedLocalMedia` rather than a path/URI to the background worker. Clone-time root-marker, ancestor, and exact-file revalidation plus handle ownership through parsing make path replacement non-retargetable and authority drift fail closed. Cursor-safe Lofty parsing uses an extension hint with property reads disabled; its explicit MP4 reread and checked raw `covr` fallback use the same handle, cap the raw file at 256 MiB, and cap returned artwork at 32 MiB. Exact art generations reject delayed results. All 9 focused tests, locked check, strict debug/release Clippy, formatting/diff checks, and complete 902-test debug/release suites pass. The direct URI helper remains transitional only for removable/external files; Radio-Browser and those two adapters keep the compound P3.1 record open at 219/223 overall and 29/30 P3. |
| 2026-07-18 | P3.1 Radio-Browser registry/view and public at-use authority (partial) | Current branch (PR pending) | Generalizes `SourceRegistry` and installs one stateless built-in Radio-Browser session whose Top Clicked, Top Voted, and Near Me feeds are exact cancellable views. Accepted snapshots expose pathless tracks while validated public locators, per-view leases, and source-wide generations remain private. Playback resolves the greatest accepted contributing generation and rechecks that exact winner through weak registry authority immediately before direct output load; same-view replacement, a newer overlapping view, removal, disconnect, and last-registry-drop all revoke pending requests. The typed client uses a known HTTPS mirror without synchronous DNS, closed redacted failures, deadlines and body caps, validated coordinates/filters/URLs, and success-empty semantics. Near Me preserves partial successful tiers, deduplicates by tier precedence before stable global distance ordering, and GTK owns only translated consent/navigation. Independent review fixed an unrelated-invalidation race during first-use consent with an exact generation-owned prerequisite marker. Locked all-target check, strict debug/release Clippy, formatting/diff checks, 53 lifecycle tests, 14 registry tests, 9 media tests, 37 radio-filtered tests, and complete 925-test debug/release suites pass. Removable and external-file at-use adapters keep the compound record open at 219/223 overall and 29/30 P3. |
