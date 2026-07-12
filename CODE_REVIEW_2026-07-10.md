# Tributary — Holistic Code Review

- **Date:** 2026-07-10
- **Version reviewed:** 0.5.0
- **Commit:** `598b332d31c6206aea620aa951b78335e4d659ed`
**Scope:** Full Rust source tree, database migrations, network backends, audio outputs,
GTK state management, packaging, CI/release workflows, and user-facing metadata.

## Executive summary

The codebase is thoughtfully organized and statically clean, but the reviewed commit is
not ready for release. No critical memory-safety defect was found. The highest risks are:

- data corruption or library metadata loss during migration and filesystem scanning;
- DAAP and playback lifecycle bugs that invalidate sessions or select the wrong track;
- credential exposure through redirects, logs, and broad authenticated stream URLs;
- destructive callbacks retained by recycled GTK sidebar rows; and
- release-pipeline integrity and reproducibility problems.

The project has strong lint discipline, useful pure-logic tests, structured backend errors,
bounded Subsonic concurrency, parser depth limits, fuzzing, and generally clear module
documentation. Its largest quality gap is integration coverage around migrations,
filesystem failures, HTTP behavior, GTK widget recycling, and real output state machines.

## Verification performed

- `cargo fmt --all -- --check` — passed.
- `cargo test --all-targets` — 152 tests passed.
- `cargo test --release` — 152 tests passed.
- `cargo clippy --all-targets -- -D warnings` — passed.
- `cargo clippy --release -- -D warnings` — passed.
- `appstreamcli validate --no-net` — passed.
- `desktop-file-validate` — warned that the `Audio`/`Music` categories require
  `AudioVideo`.
- `cargo audit` — failed with two current vulnerabilities and three warnings.
- The playlist migration failure was independently reproduced against SQLite.
- The worktree remained clean throughout the review.

The review was source- and test-based. It did not include interactive testing against real
GTK desktops, media servers, Chromecast/AirPlay/MPD receivers, USB devices, or each target
operating system.

## High-priority findings

### H1. Playlist migration can corrupt ordering and block database startup

[`m20250104_000004_unique_entry_position.rs`](src/db/migration/m20250104_000004_unique_entry_position.rs#L22)
updates `position` while its correlated subquery reads rows already modified by that same
statement. SQLite therefore produces results that depend on row-update order.

A reproduction with entries inserted as `c:30`, `b:20`, `a:10` changed every position to
`2`; creation of the unique index then failed. SeaORM does not implicitly wrap SQLite
migrations in a transaction, so the invalid update persists even though the migration is
not recorded as applied. Subsequent starts can mutate the positions again and eventually
produce a different order.

**Impact:** an upgrade can make the local library unavailable and permanently reorder
playlist entries.

**Recommendation:** materialize the desired ranking in a temporary table or CTE that is
independent of the target update, then apply the rank update and unique-index creation in
an explicit transaction. Add an upgrade test containing duplicates and row insertion order
that differs from playlist order.

### H2. Partially unreadable library roots can lose persisted metadata

The initial scan silently discards every `WalkDir` error in
[`engine.rs`](src/local/engine.rs#L121), then treats a root as available when at least one
audio file was found under it at [`engine.rs`](src/local/engine.rs#L144).

This fails in both directions:

- If one subtree becomes unreadable but another file is readable, the root is considered
  authoritative and rows from the unreadable subtree are deleted as stale at
  [`engine.rs`](src/local/engine.rs#L218). This destroys stable IDs, dates, play counts,
  and playlist linkage.
- If a healthy directory is intentionally emptied while Tributary is closed, it is treated
  as unavailable, so its stale rows survive indefinitely.

**Recommendation:** record traversal completion and errors per configured root. Never
perform stale deletion for a root whose traversal was incomplete. Treat a successfully
scanned empty directory as authoritative, and use persisted mount/device identity or an
explicit unavailable-volume state for removable/network roots.

### H3. DAAP sessions are logged out immediately after loading tracks

DAAP success paths copy `backend.all_tracks()` and then drop the backend in
[`window.rs`](src/ui/window.rs#L496) and
[`source_connect.rs`](src/ui/source_connect.rs#L716). `DaapBackend::drop` immediately
spawns a logout request in [`daap/backend.rs`](src/daap/backend.rs#L428).

The copied stream and artwork URLs contain the same session ID. A successful library sync
therefore races an immediate logout, and playback fails once the server invalidates the
session. The explicit sidebar logout URL is never populated either.

**Recommendation:** retain a session-owning backend object in a per-source registry. Resolve
stream URLs at playback time and perform logout only on explicit disconnect or controlled
application shutdown. Avoid network side effects in `Drop`.

### H4. Playback identity is coupled to a mutable view index

[`playback.rs`](src/ui/playback.rs#L47) stores `current_pos` as an index into the visible
`SortListModel`. Sorting does not remap that index. Source changes clear it while current
audio continues, and Next/EOS with no index starts at row zero.

As a result, sorting, filtering, or changing sources during playback can make Next,
Previous, repeat-one, or automatic advance load an unrelated track. Output switching adds
another inconsistent transition: [`output_switch.rs`](src/ui/output_switch.rs#L42) stops
the current output before checking whether the selected target actually changed, constructs
an empty output, and leaves `current_pos` populated.

**Recommendation:** introduce a `PlaybackSession` containing a stable source/track ID and
queue snapshot independent of the browser model. Make reselecting the current output a
no-op, and explicitly transfer or clear the session when changing output targets.

### H5. Recycled sidebar rows retain destructive click handlers

Every list-item bind calls `connect_clicked` in
[`sidebar.rs`](src/ui/sidebar.rs#L235), while unbind only hides the action button at
[`sidebar.rs`](src/ui/sidebar.rs#L398). GTK list items are recycled, and this code also
forces remove/reinsert rebinds.

A button rebound from server A to server B can retain both handlers. Clicking B may also
delete or disconnect A. Header bindings can similarly accumulate duplicate actions.

**Recommendation:** connect once during setup and resolve the item currently bound to the
list item, or retain and disconnect each `SignalHandlerId` during unbind.

### H6. Playlist track references do not have the declared database foreign key

The playlist migration creates `track_id` at
[`m20250102_000002_create_playlists.rs`](src/db/migration/m20250102_000002_create_playlists.rs#L80)
but defines only the playlist foreign key. The SeaORM entity declares `ON DELETE SET NULL`,
but entity metadata does not alter the SQLite schema.

Deleting and rediscovering a track therefore leaves a non-null dangling ID. Meanwhile,
[`reconcile_all`](src/local/playlist_manager.rs#L333) only examines rows whose ID is null,
so the playlist item remains invisible and can never be repaired. Watcher renames currently
delete and reinsert tracks, making this reachable during ordinary file organization.

**Recommendation:** rebuild the table in a migration with
`track_id REFERENCES tracks(id) ON DELETE SET NULL`, first nulling existing dangling IDs.
Handle paired filesystem renames as path updates that preserve track identity.

### H7. Redirect handling can expose bearer credentials

Subsonic and DAAP put credentials in URL queries and use default reqwest redirect behavior.
The album-art worker likewise follows redirects for every backend's credential-bearing URL
at [`album_art.rs`](src/ui/album_art.rs#L38). Reqwest enables automatic `Referer`; on an
allowed cross-origin redirect, that header can contain the full previous URL query.

Plex and Jellyfin attempt to constrain redirects, but compare only `host_str` at
[`plex/client.rs`](src/plex/client.rs#L332) and
[`jellyfin/client.rs`](src/jellyfin/client.rs#L379). They therefore permit a port change or
HTTPS-to-HTTP downgrade while retaining custom token headers.

**Recommendation:** disable automatic `Referer` for credential-bearing clients and allow
redirects only within the exact origin: scheme, hostname, and effective port. Never allow a
TLS downgrade. Rebuild authenticated requests only after validating the target.

### H8. Release Flatpak job executes mutable upstream code with write credentials

[`release.yml`](.github/workflows/release.yml#L13) grants `contents: write` to the entire
workflow. Checkout persists credentials by default, after which the Flatpak job downloads
and executes a Python script from the mutable `flatpak-builder-tools/master` branch at
[`release.yml`](.github/workflows/release.yml#L57).

An upstream compromise can poison release artifacts and potentially recover a repository
write token from the checkout configuration. Unpinned pip packages compound the exposure.

**Recommendation:** vendor the generator or pin it to an immutable commit and verify its
checksum. Set `persist-credentials: false`, give build jobs `contents: read`, and isolate
release publication into a minimal job with write permission.

### H9. Manual release tag input is ignored

The workflow declares a `tag` input at
[`release.yml`](.github/workflows/release.yml#L6), but no checkout references it. Manual
"build tag X" runs therefore build the dispatch branch or HEAD. The Arch job derives its
version from `GITHUB_REF_NAME`, which can produce `pkgver=main`.

**Recommendation:** compute one validated build ref from the release event or manual input,
pass it to every checkout, and derive package versions from the checked-out source/tag.

## Medium-priority findings

### M1. Response body limits are bypassable with chunked transfers

The Subsonic, Jellyfin, Plex, and DAAP guards inspect only `Content-Length`, for example in
[`subsonic/client.rs`](src/subsonic/client.rs#L321), then call `json()`, `text()`, or
`bytes()` and buffer the entire response. A chunked peer can send indefinitely while
remaining inside the idle read timeout. Radio, authentication, and album-art responses have
even less coverage.

Consume response streams with a running byte count, abort when the limit is exceeded, and
apply a separate overall request deadline.

### M2. Scan/watcher handoff and directory events allow permanent drift

The watcher is installed only after the complete initial scan at
[`engine.rs`](src/local/engine.rs#L77), creating a gap where changes are missed. Event
classification at [`engine.rs`](src/local/engine.rs#L351) filters every path by audio
extension, so directory rename/removal events do nothing. A file rename is handled as
delete-plus-insert, resetting identity and metadata.

Install and buffer the watcher before enumeration, replay buffered events after the scan,
rescan affected subtrees for directory events, and perform a safe full reconciliation after
watcher overflow/errors.

### M3. AirPlay `shairport-sync` fallback cannot transmit to the receiver

[`airplay_output.rs`](src/audio/airplay_output.rs#L283) explicitly discards the selected
host and port, starts `shairport-sync -o pipe -- /dev/stdin`, then writes PCM into its stdin.
Shairport Sync is an AirPlay receiver; its pipe backend outputs received audio rather than
accepting audio to transmit.

Remove the fallback or replace it with a real RAOP sender. Move process lookup, spawn, and
pipeline teardown off the GTK main thread.

### M4. MPD output never reports successful playback state

[`mpd_output.rs`](src/audio/mpd_output.rs#L245) emits `Buffering`, but successful commands
never emit `Playing`, position, or track completion. The UI spinner can remain indefinitely,
OS playback state is wrong, and automatic advance cannot work.

Use one serialized MPD worker or persistent connection that emits authoritative status,
position, and completion events. Avoid logging the full `add "<authenticated URL>"` command.

### M5. Chromecast cancellation occurs after stale external side effects

[`chromecast_output.rs`](src/audio/chromecast_output.rs#L205) assigns a generation before
spawning, but the worker does not check it while connecting, launching the receiver, or
loading media. A superseded worker can load an old track and emit `Playing` before its first
generation check. Detached stop/play/seek operations are also unordered.

Serialize commands in one worker and check the generation before every external side effect
and event. Ensure each error transitions to `Stopped`; the current local-file resolve error
can be followed by a contradictory `Buffering` event.

### M6. Late async source results overwrite newer navigation

Playlist and radio completions render unconditionally in
[`source_connect.rs`](src/ui/source_connect.rs#L118) and `radio.rs`. If the user selects a
different source while a request is running, the late result replaces the current view.

Capture a source key/generation with each operation, cache late results, and render only when
that source remains active. The USB branch already demonstrates this guard pattern.

### M7. Broad backend credentials are materialized and sent to Cast receivers

Jellyfin and Plex place reusable user/account bearer tokens in stream URLs. The generic
`Track` model retains those URLs, and Chromecast passes the raw value to any selected LAN
receiver as `content_id`.

Prefer an app-owned, opaque, short-lived proxy URL or server-provided least-privilege stream
ticket. Keep credentials out of the generic track model.

### M8. DAAP session credentials remain visible in logs and errors

[`daap/client.rs`](src/daap/client.rs#L179) logs the raw session ID at info level. Several
reqwest errors retain the request URL containing `session-id`, despite debug URL redaction.

Omit or irreversibly fingerprint the session ID and call `without_url()` before formatting
or retaining request errors.

### M9. Smart-playlist date, limit, and live-update semantics are inconsistent

[`smart_rules.rs`](src/local/smart_rules.rs#L472) compares RFC3339 timestamps to date-only
strings lexically, so `is 2026-07-10` fails for a track on that date while `is after` may
incorrectly pass. Compound sorting is applied before limit selection, but the limit path
sorts again at [`smart_rules.rs`](src/local/smart_rules.rs#L531), discarding the requested
final order. `live_updating` is persisted but never changes evaluation behavior.

Parse dates into chrono values with explicit date/instant semantics, select/truncate before
applying the requested final sort, validate relative-date bounds, and implement or remove the
live-update option.

### M10. Playlist import/export and tag writes are not fully failure-safe

XSPF export truncates the destination before incremental writes, so an I/O failure destroys
an existing export. Import tries ambiguous metadata before an exact file path and ignores
unmatched tracks and insertion errors, allowing a silent partial playlist.

Tag writing uses a predictable sibling temp path, silently accepts invalid numeric edits,
does not apply the declared album-artist field, and can leave the temp behind on rename
failure.

Use exclusive random sibling temp files and atomic replacement, make import transactional,
prefer exact paths before fingerprint matching, preserve unmatched entries for later
reconciliation, validate every edit up front, and surface matched/unmatched/error counts.

### M11. Removable-media traversal and Flatpak permissions conflict with the feature set

USB browsing uses `WalkDir::follow_links(true)` at
[`source_connect.rs`](src/ui/source_connect.rs#L267). A removable device can contain a
symlink into the user's home, causing Tributary to traverse host files outside the selected
volume.

The Flatpak grants write access only to XDG Music and does not expose `/media/$USER` or
`/run/media/$USER`, so advertised USB browsing does not work there and tag editing outside
XDG Music is normally read-only.

Disable link following, verify canonical paths remain on the selected mount, use platform
mount APIs, and define narrow Flatpak/portal permissions for removable and custom folders.

### M12. Packaging and desktop integration metadata are inconsistent

- [`build-linux.sh`](scripts/build-linux.sh#L139) invokes a Flatpak generator absent from
  the repository and writes `cargo-sources.json` somewhere the manifest does not read.
- The standalone RPM spec still declares `Version: v0.1.0` and constructs `vv0.1.0` in
  `Source0`.
- [`Cargo.toml`](Cargo.toml#L13) enables GTK 4.16 and libadwaita 1.6 APIs, while native
  package metadata permits GTK 4.14 and libadwaita 1.5.
- [`io.github.tributary.Tributary.desktop`](data/io.github.tributary.Tributary.desktop#L6)
  declares audio MIME types but `Exec=tributary` has no `%F`/`%U`, so Linux file managers
  cannot pass selected files.
- The README advertises Rust 1.80+, while `Cargo.toml` requires 1.85 and CI tests only the
  latest stable toolchain.
- AppStream release metadata stops at 0.4.1 while the crate is 0.5.0.

Synchronize package metadata from one version source, raise runtime minima, fix the desktop
field codes/categories, vendor the Flatpak source generator, and add an MSRV CI job.

## Architectural assessment

The documented `MediaBackend` boundary is mostly declarative rather than operational. The
UI constructs concrete backends, copies `Vec<Track>` values containing authenticated URLs,
and discards backend ownership. `LocalBackend` is not constructed by the application, and
several of its trait methods return `Unsupported` or create ephemeral IDs.

A source/session registry holding `Arc<dyn MediaBackend>` plus opaque source and track IDs
would address several findings together:

1. preserve DAAP and future session lifetimes;
2. resolve fresh stream URLs only when playback begins;
3. keep bearer credentials out of generic track models and receiver devices;
4. give playback stable identity independent of UI sorting/filtering; and
5. centralize refresh, cancellation, disconnect, and error-state behavior.

This should be treated as an intentional architectural milestone rather than folded into an
isolated bug fix.

## Dependency audit status

As of 2026-07-10, `cargo audit` reports:

- `crossbeam-epoch 0.9.18` — RUSTSEC-2026-0204; upgrade to `>= 0.9.20`.
- `quinn-proto 0.11.14` — RUSTSEC-2026-0185; upgrade to `>= 0.11.15`.
- warnings for unmaintained `paste` and `proc-macro-error2`, plus the
  `anyhow 1.0.102` `downcast_mut` unsoundness advisory.

`quinn-proto` is present through reqwest's optional HTTP/3 dependency graph and is not active
in the normal build, but the lockfile audit job still fails. The current code does not call
the affected `anyhow::Error::downcast_mut` path. The lockfile should nevertheless be updated
before release, and the warnings should be tracked to their upstream dependency owners.

## Strengths

- Strict debug and release Clippy checks are clean.
- The pure logic suite is fast and covers smart rules, DMAP primitives, URL redaction,
  range parsing, MPD escaping, and several conversion edge cases.
- Network clients generally use structured errors, URL validation, and connection/read
  timeouts.
- Subsonic metadata fetching uses bounded concurrency.
- DMAP parsing has a recursion-depth guard and a weekly fuzz target.
- MPD argument escaping defends the newline-delimited protocol boundary.
- Cast file serving uses opaque random IDs rather than path-derived routes and handles byte
  range edge cases.
- Runtime playlist reordering is transactional and avoids transient unique-position
  collisions; the defect is specifically in the upgrade migration.
- GTK objects are generally kept on the main thread, with background work bridged through
  channels.

## Test gaps to close

The following deserve integration or failure-injection coverage:

- upgrading a database containing reordered or duplicate playlist positions;
- foreign-key deletion and playlist reconciliation;
- empty, unavailable, and partially unreadable library roots;
- watcher startup handoff, directory events, rename identity, and overflow recovery;
- redirects, `Referer`, chunked body caps, timeouts, and pagination using mock servers;
- DAAP session ownership and explicit disconnect;
- GTK list-item recycling and stale async result suppression;
- fake MPD state/position/EOS behavior and output switching;
- delayed/superseded Chromecast sessions;
- XSPF round trips, partial I/O failures, and duplicate fingerprints;
- tag-write validation and failure cleanup;
- USB symlink escape and platform mount enumeration; and
- packaging/release dry runs, desktop validation, and the declared minimum Rust/toolkit
  versions.

## Recommended implementation order

1. Fix and regression-test the playlist migration before producing another build.
2. Make scan reconciliation non-destructive under incomplete traversal.
3. Retain backend/session ownership and remove DAAP logout from `Drop`.
4. Introduce stable playback-session identity and repair sidebar signal lifetimes.
5. Harden authenticated redirect and bounded-response handling.
6. Lock down and make the release workflow reproducible; clear the audit failure.
7. Repair watcher identity, playlist foreign keys, and output state machines.
8. Address packaging, smart-playlist, import/export, tag-writing, and removable-media gaps.
9. Complete the source/session registry architectural milestone and add integration harnesses.
