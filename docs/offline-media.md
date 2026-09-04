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
download/progress/storage UI listed under
[P3.1](task.md#p31--offline-remote-media) in `task.md`. It is also a
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
| --- | --- | --- |
| Identity | Cache entries use the same `SourceId` + `TrackId` shape as live playback. The download engine mints or adopts a per-source `MediaKey`; it never invents a new identity kind. | New persisted identifier types, new schema migrations, on-disk naming conventions beyond `task.md` and the credential-boundary section. |
| Authority | Every cached media entry remains owned by its source. The source registry's exact-snapshot capability gates download admission, reconciliation, and retirement. A committed snapshot renders offline without a live registry round-trip; disconnect and refresh never gate playback of committed bytes. No offline bypass of the registry for admission. | Concurrent access contracts for the registry's offline catalogue; specific read-side materialisation policies. |
| Download jobs | A bounded resumable job model keyed by exact `(SourceId, TrackId)` with a durable, `fsync`'d progress journal, entity validators (`If-Range`) on every range request, opaque server caps, deterministic cancellation, and structured redacted failures. Job state survives restart; it is never memory-only. | Concrete worker pool scheduling, threading model, runtime selection, telemetry. |
| Storage | Verify-then-publish: the temp file lives in the same directory (same filesystem) as its final cache path, integrity is verified on the temp file before any rename, and publish is an atomic rename with a parent-directory `fsync`. Cross-filesystem publish is refused at admission, never emulated with copy+sync+delete. A `tracks` row may link to a cache path only when integrity passed and the file is current. | Database migrations, schema, table layout, index choice, cache placement, encryption. |
| Integrity | SHA-256 is computed over the bytes on disk and compared against an expected digest whose provenance is declared per backend (capability matrix below). A backend that advertises no digest is verified by independent double-fetch; the absence of any verification path is terminal, never a silent pass. Verification completes before publish. | Hashing algorithm extension, content-defined chunking, content-addressable stores. |
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
`task-remediation-2026-07.md` P1.6 work closed for live playback — with P1.4's
exact-origin proxy as the only credential-bearing fetch path — and offline
extends the same boundary rather than reinventing it.

The non-negotiable rules are:

1. **No credential is ever persisted.** The `tracks.cache_*` columns, the
   download-job rows, the on-disk sidecars, the GTK cache rows, the
   MPD/Chromecast/AirPlay tickets, the journal logs, the redacted failure
   messages, and the diagnostic dumps are all credential-free. Bearer URLs and
   signed requests are minted only by the exact-origin proxy and consumed only
   through its revocable opaque ticket.
2. **The redirect policy is the one recorded 2026-07-13.** Authenticated
   download clients share the `task-remediation-2026-07.md` P1.4
   exact-origin + HTTPS-only redirect policy: they must follow the redirect
   matrix that the existing redirect tests enforce, must refuse HTTPS→HTTP
   downgrades, must not forward `Referer`, and must never let a redirect
   re-route a request onto a third-party host. The radio/geolocation public
   redirect policy is **not** sufficient; offline must reuse the
   authenticated one.
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
download work is a bug. This is `task-remediation-2026-07.md` P1.6's receiver
rule restated: the ticket a receiver sees carries media, never a credential.

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
3. A track ID is opaque and bounded exactly as
   [`architecture/source-lifecycle.md`](architecture/source-lifecycle.md)
   defines it. The download engine does not parse or normalise a `TrackId`. The only
   transformation it ever applies is the one-way cache-key derivation defined
   in [Per-source layout](#per-source-layout), which feeds the exact,
   unmodified byte sequence of the ID to SHA-256 and uses a fixed-width hex
   prefix as a directory name. That derivation is not parsing: the identifier
   is never interpreted, the derived key never feeds back into identity, and
   the persisted `MediaKey` remains the original opaque pair.

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
owner. Cached bytes arrive only through the same exact-origin proxy path used
by live playback, addressed by the same opaque ticket vocabulary — the engine
never sees a URL or credential. This mirrors the receiver ticket vocabulary in
`chromecast_output.rs` and the proxy ticket vocabulary in `http_security.rs`
without re-implementing either. The engine persists those bytes as streamed,
journaled segments per the resumption rules; it never buffers a whole media
file in memory.

### Committed snapshots play without a live authority

The registry's accepted generation is consulted at exactly two points:
download admission and reconciliation/retirement. It is never a playback
precondition. A committed snapshot — one whose row was published after
integrity verification — plays from local bytes while its source is
disconnected, refreshing, mid-reauthentication, or retired pending cleanup.
A disconnect or logout revokes in-flight leases only; it never unplays a
committed row. Licence state is the persisted label recorded at commit,
re-checked at the next reconciliation when the source is reachable again —
not on each offline play.

## Authenticated, resumable download jobs

A download is a one-shot operation owned by an exact accepted generation. The
job model is:

| Field | Type | Notes |
| --- | --- | --- |
| `media_key` | `MediaKey` | Bounded `(SourceId, TrackId)`; a malformed pair is terminal before any network work. |
| `capability_epoch` | `u64` | The source registry's exact accepted generation. Stale jobs retire early. |
| `requested_bytes` | `Option<u64>` | Optional hint from `Content-Length`; missing means unknown total. |
| `resume_validator` | `Option<EntityValidator>` | Strong `ETag` (preferred) or `Last-Modified` captured from the first successful response. `Some` is required for any resumption; `None` disables resume and restricts the job to full restart. |
| `current_bytes` | `u64` | Monotonic committed byte count. Durable: journaled and `fsync`'d before it is trusted as a resume point. |
| `current_sha256` | `Option<[u8; 32]>` | Engine-computed SHA-256 over the received bytes. Not trusted on its own: it is compared against the expected digest per provenance on the temp file before publish. |
| `state` | `JobState` | `Queued`, `Connecting`, `Receiving`, `Verifying`, `Committing`, `Committed`, `Failed`, `Cancelled`. |
| `last_lease` | `Option<LeaseId>` | Opaque lease reference of the in-flight HTTP request. Owned by the source registry. |
| `failure` | `Option<OfflineError>` | Redacted, structured, terminal cause when `state = Failed`. |

The rules:

1. **A job is owned by one and only one supervisor.** Local cancellation,
   source retirement, replacement, and shutdown must drain or cancel the same
   job deterministically. The supervisor can be the source registry's offline
   worker or a headless application owner on the same model as the Last.fm
   application owner composed in [#165](https://github.com/jm2/tributary/pull/165);
   it is never a GTK thread.
2. **Resumption is exact, validated, and bounded.** The durable journal —
   the job row plus an `fsync`'d sidecar recording `current_bytes` and a
   SHA-256 per committed segment — is the only trusted resume state; the
   temp file's raw on-disk length is never trusted. A resumed job truncates
   the temp file back to the journaled offset (discarding torn tail bytes
   from an interrupted write), re-verifies the last journaled segment
   digest, and re-requests the remainder with `Range` **and** `If-Range`
   carrying the captured `resume_validator`. A `206` continues the job. A
   `200` or `412` response means the entity changed or was never validated:
   the partial bytes are discarded and the job restarts from zero under the
   same job ID. Out-of-order or duplicate ranges are rejected; ranges past
   `Content-Length` are rejected. A job that captured no validator resumes
   by full restart only.
3. **Cancellation is decisive.** A user-driven cancel, lifecycle supersession,
   or shutdown cancels the in-flight lease promptly. A cancelled job leaves no
   half-promoted GTK row and no committed cache entry.
4. **Failure is structured.** `OfflineError` is a typed enum with redacted
   variants (`Network`, `AuthExpired`, `LeaseRevoked`, `IntegrityMismatch`,
   `IntegrityUnverifiable`, `LicenceDenied`, `QuotaExceeded`,
   `StorageUnavailable`, `UnsupportedSource`). No raw HTTP status, redirect
   path, body excerpt, header value, or URL parameter appears in the failure.
5. **One job per `(media_key, capability_epoch)`.** A newer request that wants
   to replace an in-flight predecessor waits for its terminal state. Replacing
   is not a separate operation; it is a new job after the predecessor reaches
   a terminal state or supersedes its capability_epoch.
6. **Job state is durable.** Jobs are persisted rows, not memory objects. A
   process restart re-derives `Queued`/`Receiving`/`Verifying` state from
   the journal and either resumes (validator present) or restarts cleanly.
   No offline job exists only in RAM.

The download engine and the source registry both treat the lease as opaque
identity. The lease is acquired exactly once per job, and the same opaque
ticket that the registration uses to address the live resource is what the
download reuses.

## Atomic storage

A cached file moves from request admission to playable media through six
explicit steps. The order is normative: **verification always completes
before the rename**, and the temp file always lives in the destination
directory.

1. **Cache-key derivation and temp reservation.** The job derives the
   bounded, filesystem-safe cache key (see
   [Per-source layout](#per-source-layout)) and creates the temp file
   **in the same directory as the final cache path** —
   `<final_name>.part-<job-id>` — so that publish is a same-filesystem
   rename. The temp name is generated by the cache engine, is short-lived,
   and is never derived from a URL or credential. If the engine cannot
   create the temp beside the final path (different filesystem, read-only
   parent), the job fails `StorageUnavailable` at admission. There is no
   cross-filesystem publish path: copy + sync + delete is explicitly
   **not** an acceptable substitute for `rename`, because it is neither
   atomic nor crash-safe.
2. **Receive.** The first reception opens the temp file with
   `truncate(true)`. A resume opens the existing temp **without** truncate,
   and only after journal validation per the resumption rules above. Every
   committed segment is hashed into the journal before its bytes count as
   progress.
3. **Finalize.** When the last byte is received, the file is `fsync`'d.
   Nothing is visible at the cache path yet.
4. **Verify on the temp file.** The engine re-computes SHA-256 from the
   bytes actually on disk and evaluates it against the expected digest per
   the provenance rules of the capability matrix: a backend-advertised
   digest must match exactly; when the backend advertises none, an
   independent second fetch (fresh authenticated request, full re-read)
   must produce an identical digest. A mismatch — or the inability to
   obtain any verification path — unlinks the temp file and fails the job
   terminally (`IntegrityMismatch`, or `IntegrityUnverifiable` when no
   digest source exists at all). No rename has occurred at this point.
5. **Publish by atomic rename.** The verified temp file is renamed onto the
   cache path — atomic because temp and final path share a directory and
   therefore a filesystem. On Unix, the parent directory is `fsync`'d after
   the rename so the published name survives power loss. Windows uses
   `FlushFileBuffers` followed by `MoveFileEx` with
   `MOVEFILE_REPLACE_EXISTING`.
6. **Commit.** Only after a successful rename does the cache row exist.
   The row records the `MediaKey` → cache-path mapping, the engine-computed
   digest, the digest provenance used, and the licence label at commit.

Failure at any step:

- Temp reservation: the previous temp is unlinked, no cache row created.
- Receive: `OfflineError::StorageUnavailable` if the filesystem refuses the
  temp or the append.
- Finalize `fsync`: `OfflineError::StorageUnavailable`.
- Verify: `OfflineError::IntegrityMismatch` on digest mismatch;
  `OfflineError::IntegrityUnverifiable` when no provenance tier can supply
  an expected digest. The temp file is unlinked; no cache row is created;
  nothing was ever renamed.
- Publish: `OfflineError::StorageUnavailable`. A failed rename leaves the
  temp in place for cleanup and the cache path untouched.

A half-promoted cache row that points at a missing or partial file is a bug
that the contract forbids; downstream layers must never observe it. The
`tracks` row remains untouched until step 6 succeeds, and the lookup path
between admission and publish returns the live endpoint only.

### Per-source layout

The cache is split by exact `SourceId`, never by backend string or base URL:

- `<cache_root>/<source_key>/<track_key>/`

`source_key` and `track_key` are **derived cache keys**, not the raw
identifiers: each is the first 32 hex characters (128 bits) of
`SHA-256(identifier_bytes)`. The identifiers are fed to the hash as their
exact, unmodified byte sequences — the engine still never parses,
normalises, or interprets them. The result is bounded (fixed length),
fixed-charset (`[0-9a-f]`), free of path separators, incapable of `..`
traversal, stable across runtimes, and reveals nothing about the identifier
it was derived from. Raw `TrackId` bytes — which may contain `/`, `..`,
unicode, or control characters — never appear in a path.

The durable `MediaKey` → cache-path mapping is recorded in the cache row at
commit; lookups are table-driven. No code path reconstructs a cache path
from an identifier except through this recorded mapping, and no URL or
credential is recoverable from a location.

The file name inside `<track_key>/` is an implementation-chosen,
credential-free constant — the directory is the per-track scope, so the
name carries no identity beyond the recorded mapping. Temp files in that
directory follow the `<final_name>.part-<job-id>` shape required by
[Atomic storage](#atomic-storage).

This layout extends the per-track identity policy in
[`source-scoped-playlists.md`](source-scoped-playlists.md) and strengthens
the archival rule that no URL or path may be reconstructed from a track's
location.

## Server capability matrix

Adapters opt in by returning `Some(OfflineSnapshot)` from
`offline_snapshot`. Each adapter documents which download path it provides:

| Backend | Download path | Snapshot cap | Expected-digest provenance | Restrictions |
| --- | --- | --- | --- | --- |
| Subsonic | `GET .../download?view=...&id=<trackId>` authenticated through the exact-origin proxy. | Per-source byte total bounded at the source-adapter-declared cap; offline rows are still capped by the per-track quota. | None advertised by the API — double-fetch verification. | Bearer URL handling per `task-remediation-2026-07.md` P1.6 — only the proxy ticket ever reaches GTK. |
| Jellyfin | `GET /Items/<id>/Download` authenticated through the exact-origin proxy. | Identical. | None guaranteed by the API — double-fetch verification. | Same. |
| Plex | `GET /library/parts/<partId>` authenticated through the exact-origin proxy; uses `X-Plex-Token` only inside the proxy boundary. | Identical. | None advertised by the API — double-fetch verification. | Same. |
| DAAP | `DAAP.song` request, authenticated through the DAAP protocol-specific lane already retired to the source lifecycle. | Identical. | None advertised by the protocol — double-fetch verification. | DAAP connection still has exactly-once logout; committed cache rows survive disconnect — logout revokes only the in-flight lease. |
| Radio-Browser | Disallowed. | — | — | Streams are public and not licensable for offline by default; deny hard. |
| Built-in local | Disallowed. | — | — | Local files are already local; the cache is the filesystem. |
| Removable | Disallowed. | — | — | Lifecycle-bound, not credentialed; the mount is the offline storage. |
| External-file | Disallowed. | — | — | One-shot ephemeral session; no persistence. |

The matrix above is normative. A new adapter that wants to opt in files a
follow-up ADR that adds a row, defines its path, and explains why the
credential-isolation argument holds for that path.

**Digest provenance tiers.** An expected digest may come from exactly two
places, in this order:

1. **Advertised digest.** A digest whose field, header, or API property is
   named in this matrix and documented in the adapter. When present it is
   compared exactly against the engine-computed SHA-256; a mismatch is
   terminal.
2. **Double-fetch verification.** When no digest is advertised, the engine
   issues a fresh authenticated request for the same resource after the
   first transfer completes and requires the SHA-256 of both transfers to
   be identical. A disagreement, or a second transfer that cannot complete,
   is terminal. The re-read is bounded by the same admission caps as the
   first transfer, and the offline quota is charged once — for the
   committed bytes, not per fetch.

A backend with neither tier cannot be downloaded from: the job fails
`IntegrityUnverifiable` before any byte is promoted. "Probably an ETag" is
not provenance; an adapter that wants to promote an `ETag` to a content
digest must name that contract in an ADR row of this matrix first.

## Credential handling

This section is normative. It repeats `task-remediation-2026-07.md` P1.6's
credential-isolation rules — minted only through P1.4's exact-origin proxy —
restated for offline storage:

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
   the source cannot provide without `TRIBUTARY_*` build-time setup remains
   disabled at runtime (see `roadmap.md:289`); the cache engine does not invent
   a way around that gate.

## Licensing

Cached media is licensed only when the source declares it. The licence model
is small and absolute:

| OperationalLicence | Meaning | Visible to GTK | Persistent |
| --- | --- | --- | --- |
| `Denied` | Default. Admission is refused before any network work, so no cache row exists and the denial itself is not persisted. | "Offline unavailable" for that source. | No |
| `SourceDeclared` | The source declares a contract that allows offline replay of its content for the user. | The licence label only. | Label only, never the text |
| `Revoked` | The source or backend has changed the licence after a row was committed. | "Licence revoked" for that row. | Row retired |

Rules:

1. The cache layer reads the current `OperationalLicence` at download
   admission and again at each reconciliation in which the source is
   reachable. Offline playback of a committed row relies on the persisted
   licence state recorded at commit and never blocks on a live read (see
   [Committed snapshots play without a live
   authority](#committed-snapshots-play-without-a-live-authority)). A
   `Revoked` row is retired at the reconciliation that observes the
   revocation, without deleting the file; the file is the user's, but the
   row is no longer a playable offline row.
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
   `tracks` integrity-as-unlink authority that `task-remediation-2026-07.md` P2.3 closes.
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
| --- | --- |
| Authenticated remote HTTP returns 401/403 mid-download | Lease revokes; job enters `Failed(AuthExpired)`. Cache row not promoted. |
| Bytes received but integrity check fails | `Failed(IntegrityMismatch)`. Temp file unlinked; nothing was renamed. |
| No digest provenance tier available for a backend | `Failed(IntegrityUnverifiable)` before publish; terminal. |
| Second transfer disagrees with the first (double-fetch) | `Failed(IntegrityMismatch)`. Temp file unlinked. |
| `OperationalLicence = Denied` or `Revoked` at admission | Job refused before network work. |
| Source retired mid-download | Job cancels; lease revokes; cache row not promoted. |
| Quota exceeded before commit | Job fails terminally with `QuotaExceeded`. |
| Filesystem refuses temp reservation | `Failed(StorageUnavailable)`. |
| User cancels a download | `Cancelled`. Temp unlinked. |
| Two requests for the same `MediaKey` race | Newest waits for terminal state of predecessor; admission is one-at-a-time. |
| Network dies between two byte ranges | Resumable; the resumed range request revalidates the entity with `If-Range` and continues from the journaled offset. A `200`/`412` answer discards partial bytes and restarts from zero. |
| Radio-Browser adapter receives an offline request | `Err(Denied)` from the capability; no network work. |
| Local file is requested for offline | `None` from the capability; no offline layer is created; the file is already local. |

## Migration plan

The implementation of this contract adds **one** migration introducing two
tables: the offline cache table and the durable download-job table. The
exact schema, indexes, and triggers are deliberately left for the
implementation record. The migration:

1. Creates the cache table keyed by the derived cache key
   (`source_key`, `track_key`) — an identity in its own right, **not** a
   strict foreign key on `tracks(id)`. A nullable advisory link to
   `tracks(id)` may exist for UI join convenience, but the cache row must
   remain valid when the track's catalogue row is absent, replaced by a
   refresh, or never materialised: a remote track's offline snapshot exists
   independent of any local `tracks` row.
2. Creates the download-job table carrying the full job model — including
   the journaled offset, the captured `resume_validator`, and the digest
   provenance in use — so that job state is durable across process
   restarts. No offline job is memory-only.
3. Persists no row that points at a missing or partial file. Promotion to a
   cached row is exactly the moment the verified rename (step 6 of Atomic
   storage) succeeds.
4. Is reversible in the same way as migration 13: any error restores the
   complete predecessor schema and data so the upgrade remains retryable.

A successful migration raises the application schema version by exactly one.

## Validation strategy

Each slice lands with its own focused regression suite. The slices are:

| Slice | Coverage |
| --- | --- |
| Identity | Same `SourceId` + `TrackId` semantics as live; no second identity kind minted. Derived cache keys: fixed hex charset and width, no separators or traversal, byte-exact identifier input. |
| Capability | Default-deny behaviour for adapters that opt out; Subsonic/Jellyfin/Plex/DAAP opt in. |
| Resumable job | Bounded, `If-Range`-validated range requests; `200`/`412` restarts from zero; journal survives crash (offset truncation, last-segment digest re-check); no-validator jobs restart only. |
| Atomic storage | Same-directory temp reservation; verify-before-publish ordering; same-filesystem rename with parent-directory `fsync`; cross-filesystem publish refused. |
| Digest provenance | Advertised digest compared exactly; double-fetch fallback equality; no-tier backends fail `IntegrityUnverifiable` before publish. |
| Credential boundary | No credential in metadata, file name, sidecar, log, or GTK row. Isolation scope per `task-remediation-2026-07.md` P1.6; redaction mechanics per P1.4. |
| Redirect policy | Per `task-remediation-2026-07.md` P1.4 matrix; HTTPS-only, no `Referer`, no HTTPS→HTTP downgrade. |
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
   remains under `task-remediation-2026-07.md` P1.5; offline extends it without
   changing it.

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
- [`task-remediation-2026-07.md`](task-remediation-2026-07.md) — P1.4
  exact-origin redirects, P1.5 response limits, P1.6 receiver credentials,
  P2.3 tag-write hardening. The credential-handling, redirect-policy,
  and unlink-authority rules in this document all cite items from that file.
- [`source-scoped-playlists.md`](source-scoped-playlists.md) — identity
  boundary for regular playlist entries; offline cache rows share the same
  identity shape.
- [`subsonic-playlist-sync.md`](subsonic-playlist-sync.md) — read authority
  lane; the offline capability reuses the same accepted-session guard model.
- [`architecture/source-lifecycle.md`](architecture/source-lifecycle.md) —
  source identity, retirement, and redaction policy.
- [`lastfm-scrobbling.md`](lastfm-scrobbling.md) — credential-free delivery
  and redaction policy precedent; the headless application owner composed
  in [#165](https://github.com/jm2/tributary/pull/165) is the reference
  shape for the offline-job supervisor.
