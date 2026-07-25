# Source-scoped offline media contracts

This document is the design-first contract for
[P3.1](task.md#p31--offline-remote-media) and
[#11](https://github.com/jm2/tributary/issues/11). It binds the persisted
identity, authenticated/resumable download jobs, atomic storage, server
capability, credential, licensing, and reconciliation behaviour that a download
and offline-cache implementation must satisfy. The implementation record and any
driver of durable change to the offline subsystem land after this contract is
accepted.

The central rules are:

> An offline media item is identified by the same source-scoped
> `(SourceId, TrackId)` pair used by live playback. The download/cache engine
> never becomes a source of its own, and a refresh never loses identifier
> independence from the originating endpoint.
>
> Credentials and tokens are never persisted in cache metadata, never embedded in
> file names or sidecars, and never exposed to GTK, the receiver tiers, or the
> filesystem outside the existing exact-origin proxy, lease, and ticket boundary.
> The off-disk location of any credential-bearing response is a revocable lease
> owned by the source lifecycle and consumed through the same opaque
> `stream`/`artwork` ticket used by live playback.
>
> No downloaded file is promoted to playable media until a cryptographic
> integrity check has matched the bytes that the originating server returned.
> Failed integrity, partial files, licence violations, and revoked leases are
> terminal failure states that never become playable rows.

This contract is the precursor to the bounded download/cache engine and the
download/progress/storage UI listed in `task.md:1030-1033`. It is also a
companion to the
[source-scoped regular-playlist storage](source-scoped-playlists.md) contract,
the [Subsonic playlist](subsonic-playlist-sync.md) contract, the
[source lifecycle](architecture/source-lifecycle.md) decision, and the
[credential-boundary decision](#credential-boundary) below.

## Status and delivery boundary

This document is the design record required by `task.md:21-22` before any
implementation PR can land against P3.1's checkbox. It is **not** an
implementation slice: it introduces no migration, new schema, runtime worker,
GTK widget, download engine, HTTP client, or persisted cache row. The
deliberately-resolved-but-unimplemented items below establish the contract that
the download/cache engine must satisfy.

| Area | What this document decides | What is explicitly reserved |
|---|---|---|
| Identity | Cache entries use the same `SourceId` + `TrackId` shape as live playback. The download engine mints or adopts a per-source `MediaKey`; it never invents a new identity kind. | New persisted identifier types, new schema migrations, on-disk naming conventions beyond `task.md` and the credential-boundary section. |
| Authority | Every cached media entry remains owned by its source. The source registry's exact-snapshot capability gates both download admission and offline catalogue rendering. No offline bypass of the registry. | Concurrent access contracts for the registry's offline catalogue; specific read-side materialisation policies. |
| Download jobs | A bounded resumable job model keyed by exact `(SourceId, TrackId)` with monotonic local progress, opaque server caps, deterministic cancellation, and structured redacted failures. | Concrete worker pool scheduling, threading model, runtime selection, telemetry. |
| Storage | Atomic temp-then-rename with pre-rename `fsync`. Per-source directory, per-track file. A `tracks` row may link to a cache path only when integrity passes and the file is current. | Database migrations, schema, table layout, index choice, cache placement, encryption. |
| Integrity | SHA-256 over the bytes the server returned, computed post-write and re-verified on resume. No fallback heuristic; mismatched bytes are terminal. | Hashing algorithm extension, content-defined chunking, content-addressable stores. |
| Capabilities | The remote source owns a default-deny `OfflineSnapshot` capability. Only the same set of backends that opt into live `ServerPlaylist`-style read authority may opt in. Radio-Browser, removable, external-file, and built-in local sources cannot. | Adapter-specific download strategies beyond HTTP(S) `Range` and Subsonic/Jellyfin/Plex/DAAP download endpoints. |
| Credentials | Cached media may carry no credential, password, signed URL, or session cookie in metadata, file name, sidecar, log, or GTK-visible row. Bearer URLs are minted only by the existing exact-origin proxy and consumed through the same opaque revocable ticket used by live playback. | New credential storage paths, new vault tables, package or build-credential integration, distribution-time-key loading. |
| Licensing | `OperationalLicence` is a per-source opt-in declared before any download is admitted. Default is `Denied`. The source emits a structured reason when licence is denied. The catalogue carries the licence label for every offline row but never the licence text itself. | Bundle-bundled music, automatic licensing negotiation, third-party licence clearing, payment integration. |
| Reconciliation | A snapshot is the durable result of one admitted job at its committed version. Refresh creates a new sibling; it never mutates the predecessor in place, and a superseded snapshot is preserved until the new one is committed and integrity-checked. | Distributed multi-device sync, push-style update subscriptions. |
| UI | The contract covers what the UI may show: progress, byte ranges, integrity state, licence label, offline-localised status text. It deliberately does not cover widget layout. | GTK widget design, accessibility tree placement, localization strings. |

This table must not be revisited until the implementation record earns each row
back from it. Adding or removing capability rows is an ADR-level change and
belongs in a follow-up of `architecture/source-lifecycle.md`.

## Credential boundary

A downloaded file is not an ordinary file. It may originate from an
authenticated HTTP endpoint whose URL, query string, or response header carries
the user's token — and under Subsonic's plaintext auth mode, the user's actual
*password*. Publishing that data through GTK, through the file system, or
through a downstream process without a boundary is the failure mode the existing
P1.4 work closed for live playback, and offline extends the same boundary
rather than reinventing it.

The non-negotiable rules are:

1. **No credential is ever persisted.** The `tracks.cache_*` columns, the
   download-job rows, the on-disk sidecars, the GTK cache rows, the
   MPD/Chromecast/AirPlay tickets, the journal logs, the redacted failure
   messages, and the diagnostic dumps are all credential-free. Bearer URLs and
   signed requests are minted only by the exact-origin proxy and consumed only
   through its revocable opaque ticket.
2. **The redirect policy is the one recorded 2026-07-13.** Authenticated
   download clients share the P1.4 exact-origin + HTTPS-only redirect policy:
   they must follow the redirect matrix that the existing redirect tests
   enforce, must refuse HTTPS→HTTP downgrades, must not forward `Referer`, and
   must never let a redirect re-route a request onto a third-party host. The
   radio/geolocation public redirect policy is **not** sufficient; offline must
   reuse the authenticated one.
3. **No off-disk identifier survives log redaction.** Path-like logging is
   forbidden for in-flight bytes. Persisted display labels are the structured
   metadata (`title`, `artist`, `album`), never the URL or the lease.
4. **Lease recovery at use mirrors playback.** When GTK or a receiver asks the
   cache for media, the path returned is the on-disk cached file; the cached
   file is opened through the same retained-mount authority as a local file.
   Resumption of a partially downloaded file never opens a credential-bearing
   handle.

The credential-boundary section is normative and may not be weakened by an
implementation slice. Any slice that would persist a credential to make a
download work is a bug. This matches P1.4's "no token in MPD/Chromecast/AirPlay
ticket" rule verbatim.

## Source-scoped identity and lifecycle

### Identity survives download, refresh, and offline rendering

A media item destined for offline caching is identified by the same
`MediaKey { source_id: SourceId, track_id: TrackId }` shape that live playback
uses. The download engine is not a `SourceKind`; it is a state change for an
existing media row. As a consequence:

1. The download engine mints no `SourceId` and registers no adapter. Every
   cached media item is owned by an existing `Source` whose lifecycle already
   governs connection, cancellation, retirement, and shutdown.
2. The engine may extend a saved source with a new capability row for offline
   operations, but the source retains its existing identity, audit, and
   redaction behaviour.
3. A track ID is opaque and bounded exactly as it is in `task.md:78-83`. The
   download engine does not parse, normalise, or hash a `TrackId`.

### Each source owns its offline decision

A source may opt in to the offline capability, opt out, or revoke an earlier
opt-in without taking the playback catalogue with it. The contract:

1. The registry's `MediaBackend` trait is extended with a default-deny
   `offline_snapshot() -> Result<Option<OfflineSnapshot>, OfflineError>`
   adapter. `None` and `Err(Denied)` are distinct; `None` means the source has
   not declared, `Denied` means it explicitly refuses.
2. Only the same authenticated-backends that opt into live `ServerPlaylist`
   reads (Subsonic, Jellyfin, Plex, DAAP) may opt in here. Radio-Browser,
   removable, external-file, and the built-in local source must return
   `None`. A local file is already local; a removable volume is lifecycle-bound
   but not credentialed; an external file is one-shot.
3. The capability is generation-owned. Replacing a source supersedes its offline
   decision; the cache layers generate replacement on the same generation as
   the live adapter.

### Offline is not yet another credential lane

The offline engine has its own job lifecycle but does not become a credential
owner. Cached bytes arrive through the same exact-origin proxy used by live
playback; the offline engine receives completion through the same opaque
ticket and may only persist the resulting `Vec<u8>`. This mirrors the receiver
ticket vocabulary in `chromecast_output.rs` and the proxy ticket vocabulary in
`http_security.rs` without re-implementing either.

## Authenticated, resumable download jobs

A download is a one-shot operation owned by an exact accepted generation. The
job model is:

| Field | Type | Notes |
|---|---|---|
| `media_key` | `MediaKey` | Bounded `(SourceId, TrackId)`; a malformed pair is terminal before any network work. |
| `capability_epoch` | `u64` | The source registry's exact accepted generation. Stale jobs retire early. |
| `requested_bytes` | `Option<u64>` | Optional hint from `Content-Length`; missing means unknown total. |
| `current_bytes` | `u64` | Monotonic committed byte count. |
| `current_sha256` | `Option<[u8; 32]>` | Progressively trusted only after the full file is received and re-checked post-write. |
| `state` | `JobState` | `Queued`, `Connecting`, `Receiving`, `Verifying`, `Committing`, `Committed`, `Failed`, `Cancelled`. |
| `last_lease` | `Option<LeaseId>` | Opaque lease reference of the in-flight HTTP request. Owned by the source registry. |
| `failure` | `Option<OfflineError>` | Redacted, structured, terminal cause when `state = Failed`. |

The rules:

1. **A job is owned by one and only one supervisor.** Local cancellation,
   source retirement, replacement, and shutdown must drain or cancel the same
   job deterministically. The supervisor can be the source registry's offline
   worker or a headless application owner modelled on `mol-polecat-work`'s
   application-owner; it is never a GTK thread.
2. **Resumption is exact and bounded.** A retried range request uses the
   `Range` bytes the previous job last committed. Out-of-order or duplicate
   ranges are rejected; ranges past `Content-Length` are rejected; the previous
   temporary file is reused only after byte-level equality check.
3. **Cancellation is decisive.** A user-driven cancel, lifecycle supersession,
   or shutdown cancels the in-flight lease promptly. A cancelled job leaves no
   half-promoted GTK row and no committed cache entry.
4. **Failure is structured.** `OfflineError` is a typed enum with redacted
   variants (`Network`, `AuthExpired`, `LeaseRevoked`, `IntegrityMismatch`,
   `LicenceDenied`, `QuotaExceeded`, `StorageUnavailable`,
   `UnsupportedSource`). No raw HTTP status, redirect path, body excerpt,
   header value, or URL parameter appears in the failure.
5. **One job per `(media_key, capability_epoch)`.** A newer request that wants
   to replace an in-flight predecessor waits for its terminal state. Replacing
   is not a separate operation; it is a new job after the predecessor reaches
   a terminal state or supersedes its capability_epoch.

The download engine and the source registry both treat the lease as opaque
identity. The lease is acquired exactly once per job, and the same opaque
ticket that the registration uses to address the live resource is what the
download reuses.

## Atomic storage

A cached file moves from request admission to playable media through five
explicit steps:

1. **Temp reservation.** The job writes to a per-source temp directory whose
   path is **not** derived from the cache path. The temp path is generated by
   the cache engine and is short-lived.
2. **Truncate-then-stream.** The job opens the temp file with
   `OpenOptions::new().create(true).truncate(true).write(true)` and never
   re-opens it for append; the file is committed only after the full byte
   range has been received.
3. **Pre-rename `fsync`.** Before the rename the file is `fsync`'d; the parent
   directory is `fsync`'d on Unix afterwards. Windows uses `FlushFileBuffers`
   followed by a transactional move or `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`.
4. **Atomic rename.** The temp file is renamed into its cache path. The rename
   is atomic on the source filesystem; on cross-filesystem cache placements,
   the job must verify the rename is single-step or fall back to copy + sync
   + delete, never a partial overwrite.
5. **Post-write integrity check.** The job's `current_sha256` is re-computed
   from the bytes on disk; only equality with the server-advertised digest
   enables the next step.

Failure at any step:

- Temp reservation: the previous temp is unlinked, no cache row created.
- Truncate-then-stream: `OfflineError::StorageUnavailable` if the filesystem
  refuses the temp.
- Pre-rename `fsync` or rename: `OfflineError::StorageUnavailable`.
- Post-write integrity mismatch: `OfflineError::IntegrityMismatch`. The temp
  file is unlinked; no cache row is created.

A half-promoted cache row that points at a missing or partial file is a bug
that the contract forbids; downstream layers must never observe it. The
`tracks` row remains untouched until step 5 succeeds, and the lookup path
between step 1 and step 5 returns the live endpoint only.

### Per-source layout

The cache is split by exact `SourceId`, never by backend string or base URL:

- `<cache_root>/<source_id_hex>/<track_id_within_source>/`

The exact layout extends the per-track identity policy in
[`source-scoped-playlists.md`](source-scoped-playlists.md) and mirrors the
archival rule that no URL or path may be reconstructed from a track's location.

## Server capability matrix

Adapters opt in by returning `Some(OfflineSnapshot)` from
`offline_snapshot`. Each adapter documents which download path it provides:

| Backend | Download path | Snapshot cap | Restrictions |
|---|---|---|---|
| Subsonic | `GET .../download?view=...&id=<trackId>` authenticated through the exact-origin proxy. | Per-source byte total bounded at the source-adapter-declared cap; offline rows are still capped by the per-track quota. | Bearer URL handling per P1.6 — only the proxy ticket ever reaches GTK. |
| Jellyfin | `GET /Items/<id>/Download` authenticated through the exact-origin proxy. | Identical. | Same. |
| Plex | `GET /library/parts/<partId>` authenticated through the exact-origin proxy; uses `X-Plex-Token` only inside the proxy boundary. | Identical. | Same. |
| DAAP | `DAAP.song` request, authenticated through the DAAP protocol-specific lane already retired to the source lifecycle. | Identical. | DAAP connection still has exactly-once logout; cache rows must retire on disconnect. |
| Radio-Browser | Disallowed. | — | Streams are public and not licensable for offline by default; deny hard. |
| Built-in local | Disallowed. | — | Local files are already local; the cache is the filesystem. |
| Removable | Disallowed. | — | Lifecycle-bound, not credentialed; the mount is the offline storage. |
| External-file | Disallowed. | — | One-shot ephemeral session; no persistence. |

The matrix above is normative. A new adapter that wants to opt in files a
follow-up ADR that adds a row, defines its path, and explains why the
credential-isolation argument holds for that path.

## Credential handling

This section is normative. It repeats P1.4's rules, restated for offline
storage:

1. **Persistence is forbidden.** No `tracks` row carries a credential, URL,
   signed parameter, header, or token in any column. No file name or directory
   name carries one. No sidecar or metadata file carries one. No log or
   diagnostic carries one.
2. **Backing storage is the same opaque ticket.** When the cache layer needs to
   address a streamed file, it uses the existing revocable opaque proxy
   ticket. When the cached bytes move from cache to GTK or to a receiver, the
   cached file is opened through the same retained-mount file capability that
   live playback uses.
3. **Reauthentication is one-way.** If the user's token expires or is revoked
   while a download is in flight, the lease is revoked, the job enters the
   `Failed(AuthExpired)` terminal state, and the cache row is not promoted.
   The user reauthenticates through the source's normal authorization path
   and restarts the download.
4. **DAAP logout is required.** Downloading a DAAP track does not delay DAAP
   logout. The cache row is committed before the session ends; the session's
   revocation retires only the in-flight lease, not the cache row.
5. **Loaded credentials stay loaded.** Offline downloads do not load built or
   shipped credentials. A source whose authorization requires a credential that
   the source cannot provide without `GC_*` build-time setup remains
   disabled at runtime (see `roadmap.md:289`); the cache engine does not invent
   a way around that gate.

## Licensing

Cached media is licensed only when the source declares it. The licence model
is small and absolute:

| OperationalLicence | Meaning | Visible to GTK | Persistent |
|---|---|---|---|
| `Denied` | Default. The cached row exists structurally but cannot become playable. | "Offline unavailable" for that source. | No |
| `SourceDeclared` | The source declares a contract that allows offline replay of its content for the user. | The licence label only. | No stored text |
| `Revoked` | The source or backend has changed the licence after a row was committed. | "Licence revoked" for that row. | Row retired |

Rules:

1. The cache layer reads the current `OperationalLicence` at download admission
   and again at offline rendering. A `Revoked` row is retired without deleting
   the file; the file is the user's, but the row is no longer a playable
   offline row.
2. The licence label is a short, bounded identifier supplied by the source —
   e.g. `subsonic-streaming-self`. Never a free-form text field, never the
   full licence, never the URL of the licence page.
3. No bundle-bundled music, no third-party clearing, no payment integration.
   Sources that need payment integration declare it explicitly when they opt
   in.
4. Re-licensing at upgrade time is non-destructive. The new licence state
   takes effect on next admission; committed rows are not retroactively
   rewritten.

## Reconciliation

A snapshot is one durable result of one admitted job at one version. Refresh
does not mutate; it siblings. The rules:

1. **Snapshots are immutable.** A committed snapshot's bytes, hash, and source
   identity never change. Refresh creates a new snapshot; the predecessor
   remains until the new snapshot is committed.
2. **Sibling retention is bounded.** When a new snapshot is committed, the
   predecessor is queued for unlink. The unlink path goes through the same
   `tracks` integrity-as-unlink authority that Path 2.3 closes.
3. **Refresh is monotonic.** A successful refresh only retires a row when
   either the new snapshot is committed or the user explicitly chooses
   "Delete cache entry". Refreshing to detect stale content is not a delete
   trigger.
4. **Offline catalogue rendering is read-only.** Source retirement, manual
   removal, `source_unlink`, and `Unlink` invalidate the offline catalogue
   row; the underlying file is unlinked through the same path validation as
   a regular cache unlink. Stale projection work is discarded.

## Cancellation, quota, and eviction

The download/cache engine observes a single, bounded quota and eviction
policy:

1. **Quota is global.** The application has one offline quota expressed in
   bytes. Sub-limits per source are advisory only at admission time.
2. **Eviction is newest-first within source, oldest-first across sources.**
   When the quota is exceeded, eviction walks sources in oldest-cache-first
   order and within a source newest-first.
3. **Eviction is content-aware.** Evicted rows are also `Deleted` rows in the
   same transaction — no half-evicted state.
4. **Cancellation is local-failure equivalent.** A cancelled job leaves the
   same atomicity footprint as a `QuotaExceeded` failure: temp file unlinked,
   no cache row created.

## UI contract

The download/progress/storage UI must show:

1. **Progress.** Per-job byte progress and per-source aggregate progress.
2. **Integrity.** A committed row's `Committed` state, never `Verifying` or
   partial.
3. **Licence label.** The licence label per row, never the URL or the full
   licence text.
4. **Reason.** The structured redacted `OfflineError` for failed rows.
5. **No credential.** The cache rows in the GTK tree carry no URL, no
   certificate fingerprint, no token, no path that could be reverse-engineered
   into one.

The UI does not show the on-disk path. It does show the structured `SourceKind`
label (e.g. `Subsonic — example.com`) and the title/artist/album metadata
that the source itself published.

## Failure modes

This contract fixes the following failure cases:

| Situation | Behavior |
|---|---|
| Authenticated remote HTTP returns 401/403 mid-download | Lease revokes; job enters `Failed(AuthExpired)`. Cache row not promoted. |
| Bytes received but integrity check fails | `Failed(IntegrityMismatch)`. Temp file unlinked. |
| `OperationalLicence = Denied` or `Revoked` at admission | Job refused before network work. |
| Source retired mid-download | Job cancels; lease revokes; cache row not promoted. |
| Quota exceeded before commit | Job fails terminally with `QuotaExceeded`. |
| Filesystem refuses temp reservation | `Failed(StorageUnavailable)`. |
| User cancels a download | `Cancelled`. Temp unlinked. |
| Two requests for the same `MediaKey` race | Newest waits for terminal state of predecessor; admission is one-at-a-time. |
| Network dies between two byte ranges | Resumable; range request continues from `current_bytes`. |
| Radio-Browser adapter receives an offline request | `Err(Denied)` from the capability; no network work. |
| Local file is requested for offline | `None` from the capability; no offline layer is created; the file is already local. |

## Migration plan

The implementation of this contract adds **one** migration: an offline cache
table whose schema mirrors the existing playlist_entry shape, replacing
`tracks.local_track_id` semantics with a typed cache reference. The exact
schema, indexes, and triggers are deliberately left for the implementation
record. The migration:

1. Creates the cache table with bounded columns, default-deny triggers, and
   strict foreign-key references to `tracks(id)` and the source registry.
2. Persists no row that points at a missing or partial file. Promotion to a
   cached row is exactly the moment the post-write integrity check passes.
3. Is reversible in the same way as migration 13: any error restores the
   complete predecessor schema and data so the upgrade remains retryable.

A successful migration raises the application schema version by exactly one.

## Validation strategy

Each slice lands with its own focused regression suite. The slices are:

| Slice | Coverage |
|---|---|
| Identity | Same `SourceId` + `TrackId` semantics as live; no second identity kind minted. |
| Capability | Default-deny behaviour for adapters that opt out; Subsonic/Jellyfin/Plex/DAAP opt in. |
| Resumable job | Bounded range requests; out-of-order rejection; cap-bytes mismatch; checksum guard. |
| Atomic storage | `fsync` + rename; Windows transactional move; cross-filesystem fallback. |
| Credential boundary | No credential in metadata, file name, sidecar, log, or GTK row. Same regex-style coverage as P1.4. |
| Redirect policy | Per P1.4 matrix; HTTPS-only, no `Referer`, no HTTPS→HTTP downgrade. |
| Licensing | Default-deny; revocation retires rows but preserves files. |
| Reconciliation | Refresh creates a sibling; no in-place mutation; unlink is content-aware. |
| Cancellation | Lifecycle supersession cancels in-flight jobs. |
| Quota and eviction | Newest-source-first eviction; transactional unlink. |
| UI | Credential-free GTK rows; localised progress and failure. |

The contract does not bless a single language binding or test framework; it
specifies the behaviour the suite must cover. The implementation record picks
the framework.

## Open scope deliberately deferred

The P3.1 record also accepts the following work that this contract does not
own:

1. **Encrypted cache.** Filesystem-level encryption of cached bytes is a
   separate enhancement. Until it lands, the offline cache lives behind the
   same atomic-temp-then-rename guarantee that local file writes already
   follow.
2. **Licence clearing.** Third-party licence clearing and payment flows belong
   to the source adapter, not the cache engine.
3. **Cross-device sync.** Pushing a cached snapshot to another device is a
   separate ADR — see [P3.2](task.md#p32--android-and-device-synchronization).
4. **Distributed quota.** Cross-process quota enforcement is out of scope.
5. **DAAP-only authorization refresh.** DAAP's particular reauthorization flow
   remains under P1.5; offline extends it without changing it.

The contract exists to make those deferred areas explicit, so an implementer
does not have to invent them mid-slice.

## Compatibility and abandonment

Until an offline-capable source opts in for the first time, none of the offline
machinery is exercised at runtime. A database that has never had an offline
cache row is identical at the byte level to a database without the migration.
A source that opts out keeps the same `OfflineSnapshot::None` semantics that
the new adapter implies.

When the project eventually retires the offline subsystem, the migration is
reversed by a follower migration that drops the offline tables, indexes, and
triggers in one transaction; no live production path depends on the offline
machinery existing.

## See also

- [`task.md`](task.md) — P3.1 implementation record and overall backlog.
- [`source-scoped-playlists.md`](source-scoped-playlists.md) — identity
  boundary for regular playlist entries; offline cache rows share the same
  identity shape.
- [`subsonic-playlist-sync.md`](subsonic-playlist-sync.md) — read authority
  lane; the offline capability reuses the same accepted-session guard model.
- [`architecture/source-lifecycle.md`](architecture/source-lifecycle.md) —
  source identity, retirement, and redaction policy.
- [`lastfm-scrobbling.md`](lastfm-scrobbling.md) — credential-free delivery
  and redaction policy precedent.
