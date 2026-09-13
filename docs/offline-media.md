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
| Identity | Cache entries use the same `SourceId` + `TrackId` shape as live playback. The download engine adopts the live per-source `MediaKey`; it never invents a new identity kind. | New persisted media identifier kinds, new schema migrations for media identity, on-disk naming conventions beyond `task.md` and the credential-boundary section. The durable `SourceIncarnationId` of [restart authorization](#authenticated-resumable-download-jobs) is a registry-side durable field, not a media identifier kind; pre-existing saved sources receive one stable, persisted value at first load. |
| Authority | Every cached media entry remains owned by its source. The source registry's exact-snapshot capability gates download admission, reconciliation, and retirement. A committed snapshot renders offline without a live registry round-trip; disconnect and refresh never gate playback of committed bytes. No offline bypass of the registry for admission. | Concurrent access contracts for the registry's offline catalogue; specific read-side materialisation policies. |
| Download jobs | A bounded resumable job model keyed by exact `(SourceId, TrackId)` with a durable, `fsync`'d progress journal, entity validators (`If-Range`) on every range request, opaque server caps, deterministic cancellation, and structured redacted failures. Job state survives restart; it is never memory-only. Restart authorization is by durable source-incarnation identity (`SourceId` + `SourceIncarnationId`), never by the transient accepted-generation number, applied consistently to lease reacquisition, resumption, and publish-intent adoption. | Concrete worker pool scheduling, threading model, runtime selection, telemetry. |
| Storage | Verify-then-publish: the temp file lives in the same directory (same filesystem) as its final cache path, integrity is verified on the temp file before any rename, and publish is an atomic rename — durability-ordered on Unix by a parent-directory `fsync` chain that re-derives and re-syncs the complete ancestor chain on every attempt, and on Windows by the documented `MOVEFILE_WRITE_THROUGH` barrier ([Atomic storage](#atomic-storage)). The final path is snapshot-scoped, so a refresh publishes a sibling instead of overwriting a predecessor's bytes; a journaled publish intent makes the rename-to-commit window crash-recoverable — adoption at startup completes the pending publication barrier, re-syncing the directory entry for the already-renamed file, before it may insert the row or clear the intent — and a durable delete intent — the publish intent itself, or the `fsync`'d terminal-verdict record that supersedes it on a post-rename terminal transition — is the delete owner for a file published without a row. Cross-filesystem publish is refused at admission, never emulated with copy+sync+delete. A `tracks` row may link to a cache path only when integrity passed, the per-track cap held before the rename, and the file is current. | Database migrations, schema, table layout, index choice, cache placement, encryption. |
| Integrity | SHA-256 is computed over the bytes on disk and compared against an expected digest whose provenance is declared per backend (capability matrix below). A backend that advertises no digest is verified by independent double-fetch; the absence of any verification path is terminal, never a silent pass. Verification completes before publish. | Hashing algorithm extension, content-defined chunking, content-addressable stores. |
| Capabilities | The remote source owns a default-deny `OfflineSnapshot` capability. Only the same set of backends that opt into live `ServerPlaylist`-style read authority may opt in. Radio-Browser, removable, external-file, and built-in local sources cannot. | Adapter-specific download strategies beyond HTTP(S) `Range` and Subsonic/Jellyfin/Plex/DAAP download endpoints. |
| Credentials | Cached media may carry no credential, password, signed URL, or session cookie in metadata, file name, sidecar, log, or GTK-visible row. Bearer URLs are minted only by the existing exact-origin proxy and consumed through the same opaque revocable ticket used by live playback. | New credential storage paths, new vault tables, package or build-credential integration, distribution-time-key loading. |
| Licensing | `OperationalLicence` is a per-source opt-in declared before any download is admitted. Default is `Denied`. The source emits a structured reason when licence is denied. The catalogue carries the licence label for every offline row but never the licence text itself. | Bundle-bundled music, automatic licensing negotiation, third-party licence clearing, payment integration. |
| Reconciliation | A snapshot is the durable result of one admitted job at its committed version. Refresh creates a new sibling; it never mutates the predecessor in place, and a superseded snapshot is preserved until the new one is committed and integrity-checked. Superseded snapshots retire through a staged delete: durable tombstone first, idempotent unlink after. | Distributed multi-device sync, push-style update subscriptions. |
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
   reads (Subsonic, Jellyfin, Plex, DAAP) may opt in here. Removable,
   external-file, and the built-in local source must return `None`: a local
   file is already local; a removable volume is lifecycle-bound but not
   credentialed; an external file is one-shot. Radio-Browser must return
   `Err(Denied)`: its streams are public and not licensable for offline by
   default, so it explicitly opts out instead of leaving the capability
   undeclared.
3. The capability is incarnation-owned and bound to the source's durable
   incarnation identity (see
   [restart authorization](#authenticated-resumable-download-jobs)):
   replacing a source mints a new incarnation and supersedes its offline
   decision, while a process restart that only resets the transient operation
   and session generation counters leaves the durable incarnation — and
   therefore an in-flight job's restart authorization — unchanged. The cache
   layers follow the live adapter's incarnation. `SourceIncarnationId` is a
   durable, non-secret field on the registry's saved-source record,
   distinct from `SourceId` (which a replacement preserves) and from the
   transient session epoch and operation generation (which are never
   persisted).

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
| `source_incarnation` | `SourceIncarnationId` | Durable, non-secret identity of the owning source incarnation, recorded at admission. Restart authorization compares this durable identity, never a transient generation number. |
| `capability_epoch` | `u64` | The source registry's accepted generation, process-local ordering only. A stale predecessor within one run retires early; it is never restart-stable and never authorizes a restart (see [restart authorization](#authenticated-resumable-download-jobs)). |
| `requested_bytes` | `Option<u64>` | Optional hint from `Content-Length`; missing means unknown total. |
| `resume_validator` | `Option<EntityValidator>` | Strong `ETag` (preferred) or `Last-Modified` captured from the first successful response. `Some` is required for any resumption; `None` disables resume and restricts the job to full restart. |
| `current_bytes` | `u64` | Monotonic committed byte count. Durable: journaled and `fsync`'d before it is trusted as a resume point. |
| `current_sha256` | `Option<[u8; 32]>` | Engine-computed SHA-256 over the received bytes. Not trusted on its own: it is compared against the expected digest per provenance on the temp file before publish. |
| `state` | `JobState` | `Queued`, `Connecting`, `Receiving`, `Verifying`, `Committing`, `Committed`, `Failed`, `Cancelled`. |
| `last_lease` | `Option<LeaseId>` | Opaque lease reference of the in-flight HTTP request. Owned by the source registry and process-local only: it is never persisted across a restart, and a restarted job reacquires under the restart-authorization rule below. |
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
   temp file's raw on-disk length is never trusted. A segment's bytes are
   made durable **before** the journal records that segment's progress —
   the temp file is flushed to the disk for the appended range first, and
   the sidecar record is `fsync`'d second — so the journal never runs
   ahead of the bytes it certifies. A resumed job truncates the temp file
   back to the journaled offset (discarding torn tail bytes
   from an interrupted write), re-verifies the last journaled segment
   digest, and re-requests the remainder with `Range` **and** `If-Range`
   carrying the captured `resume_validator`. A `206` continues the job. A
   `200` or `412` response means the entity changed or was never validated:
   the partial bytes are discarded and the job restarts from zero under the
   same job ID. Out-of-order or duplicate ranges are rejected; ranges past
   `Content-Length` are rejected. A job that captured no validator resumes
   by full restart only. If the temp file is **shorter** than the journaled
   offset, or the last journaled segment digest does not match the bytes
   actually on disk, the offset is not trusted: the journaled progress is
   discarded and the job restarts from zero under the same job ID — the
   digest evidence, not the offset, decides.
3. **Cancellation is decisive.** A user-driven cancel, lifecycle supersession,
   or shutdown cancels the in-flight lease promptly. A cancelled job leaves no
   half-promoted GTK row and no committed cache entry.
4. **Failure is structured.** `OfflineError` is a typed enum with redacted
   variants (`Network`, `AuthExpired`, `LeaseRevoked`, `Denied`,
   `IntegrityMismatch`, `IntegrityUnverifiable`, `LicenceDenied`,
   `QuotaExceeded`, `StorageUnavailable`, `UnsupportedSource`). `Denied` is
   the capability-level refusal surfaced by `offline_snapshot` for a source
   that explicitly opts out; it is distinct from `LicenceDenied`, which is
   the operational-licence verdict. No raw HTTP status, redirect
   path, body excerpt, header value, or URL parameter appears in the failure.
5. **One job per `(media_key, source_incarnation)`.** A newer request that
   wants to replace an in-flight predecessor waits for its terminal state.
   Replacing is not a separate operation; it is a new job after the
   predecessor reaches a terminal state or its durable incarnation is
   superseded. `capability_epoch` only orders supersession within one process
   run; it never defines job identity across a restart.
6. **Job state is durable.** Jobs are persisted rows, not memory objects. A
   process restart re-derives `Queued`/`Receiving`/`Verifying` state from
   the journal, resolves a journaled publish intent per the
   [publish intent and restart recovery](#publish-intent-and-restart-recovery)
   protocol, and either resumes (validator present) or restarts cleanly —
   after reacquiring the lease from the source's current accepted generation
   under the restart-authorization rule below. No offline job exists only in
   RAM.

The download engine and the source registry both treat the lease as opaque
identity. A `MediaLease` is process-local: it lives no longer than the
process that acquired it and is never persisted. Within one process, the
lease for an in-flight transfer is acquired exactly once per job, and the
same opaque ticket that the registration uses to address the live resource
is what the download reuses.

**Restart authorization is by durable source-incarnation identity, never by a
transient generation number.** The registry persists, per durable source, a
non-secret `SourceIncarnationId`: minted with the source's durable
registration and re-minted only when the source is replaced by a genuinely
different incarnation — a new endpoint, adapter identity, or saved
configuration. A mere process restart resets the transient operation
generation and session epoch counters but leaves `SourceIncarnationId`
unchanged; it is a durable identity, not an epoch or generation, and carries
no credential, locator, or route. The job records the `SourceIncarnationId`
it was admitted against; the `capability_epoch` it also records is
process-local ordering only and is never restart-stable. A persisted job that
survives a process restart therefore holds no lease, and before any byte
moves it rebinds by durable `SourceId` + `SourceIncarnationId`:

- **Same durable incarnation.** The job reacquires a fresh, process-local
  lease from the source's current accepted generation — whatever its numeric
  value — and revalidates the media identity, the `offline_snapshot`
  capability and licence state, and the captured `resume_validator` against
  that fresh generation. A changed transient generation, so that
  `capability_epoch` differs from the new run's counter, is expected here and
  is not a rejection.
- **Different durable incarnation.** The source was replaced; the persisted
  job is stale and resolves terminally under its ordinary rules. It never
  resumes, even when the new run's transient generation is numerically equal
  to the job's recorded `capability_epoch`: numeric equality is never
  authorization.
- **Rebinding failure for any other reason.** Source absent, capability
  withdrawn or denied, licence revoked, authorization expired, or validator
  rejected resolves per the job's ordinary terminal rules; it never resumes
  on a stale lease.

No credential, token, ticket material, or process-local lease handle is
persisted at any point. The persisted `SourceIncarnationId` is a durable
non-secret identifier and carries no credential, locator, or route.

**Pre-existing saved sources are incarnated once at first load and never
re-minted.** An installation that predates this contract holds version-1
`servers.json` rows carrying only `type`, `name`, `url`, and `source_id`;
such a row has no durable incarnation, and leaving the field merely declared
would let an implementation re-mint one on every load — breaking every
resumed job's restart authorization — or declare the source replaced and
retire its jobs. The contract therefore fixes a first-load rule: the
migration that introduces the offline subsystem assigns each pre-existing
saved source a durable `SourceIncarnationId` and persists it in the same
saved-source record, exactly as a newly registered source would receive
one. The assignment is idempotent and derivation-stable: the value is a
deterministic function of the row's durable identity fields (`source_id`,
`type`, and `url`), so a retry, a crash before the durable write lands, or a
concurrent load converges on a single value instead of racing to mint
several, and the persisted value, once written, is the authority. A later
change to any of those identity fields is a genuine replacement under the
re-minting rule above and mints a new incarnation; an unchanged row reloaded
after a process restart keeps the incarnation it was assigned, so a resumed
job's `SourceId` + `SourceIncarnationId` still rebinds. Until a source's
incarnation is persisted, no offline job for that source is admitted,
resumed, or adopted. The assigned value is itself opaque and non-secret: it
carries no credential, locator, or route, and it is not a display name.

## Atomic storage

A cached file moves from request admission to playable media through six
explicit steps. The order is normative: **verification always completes
before the rename**, and the temp file always lives in the destination
directory.

1. **Cache-key derivation and temp reservation.** The job derives the
   bounded, filesystem-safe cache key (see
   [Per-source layout](#per-source-layout)), creates every missing
   ancestor directory of the snapshot-scoped final path — top-down, under
   the ordinary creation rules — and then creates the temp file
   **in the same directory as the final cache path** —
   `<final_name>.part-<job-id>` — so that publish is a same-filesystem
   rename. The ancestors are created here because the temp must live in
   that directory from the first received byte; no durability is claimed
   for the new entries at this step — making them durable is step 5's
   ordered work, before the rename. The temp name is generated by the
   cache engine, is short-lived,
   and is never derived from a URL or credential. If the engine cannot
   create an ancestor or the temp beside the final path (different
   filesystem, read-only parent),
   the job fails `StorageUnavailable` at admission. There is no
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
   digest source exists at all). The verified size is also checked against
   the per-track cap here, before any rename: an over-cap snapshot fails
   `QuotaExceeded` and the temp is unlinked. No rename has occurred at
   this point.
5. **Publish by atomic rename.** The engine first appends an `fsync`'d
   publish-intent record to the job's durable journal, naming the
   snapshot-scoped final cache path and the verified digest (see
   [Publish intent and restart recovery](#publish-intent-and-restart-recovery)).
   It then renames the verified temp file onto that path — a path no
   committed snapshot owns, so the rename cannot overwrite a predecessor's
   bytes. The rename is atomic because temp and final path share a
   directory and therefore a filesystem.
   Durability requires the whole ancestor chain, not just the file's own
   entry: the final path lives at
   `<cache_root>/<source_key>/<track_key>/<snapshot_key>/`
   ([Per-source layout](#per-source-layout)), and step 1 created every
   missing ancestor before the temp reservation. The durability pass must
   not depend on this process remembering which entries it created: after a
   crash the resumed process did not run step 1, and the previous process's
   set of created directories died with it. Instead the engine **re-derives
   the complete ancestor chain from the recorded final path and re-syncs
   every entry in it unconditionally, top-down and idempotently**, before
   the rename: `<cache_root>`'s entry for `<source_key>`, then
   `<source_key>`'s entry for `<track_key>`, then `<track_key>`'s entry for
   `<snapshot_key>`, and (after the rename) the snapshot directory's own
   entry for the published file. On Unix an entry is made durable by
   `fsync`-ing the directory that contains it — `<cache_root>` for the
   source entry, then each derived directory in turn — and the snapshot
   directory is `fsync`'d after the rename, so after the publish the file's
   entry and the whole ancestor chain up to `<cache_root>` survive power
   loss. Re-syncing entries that already exist is harmless and idempotent,
   so the pass is equally correct on the first publish, on resume after a
   crash at any point in or before it, and after a power loss that occurred
   before the previous process recorded anything; the engine never trusts
   the previous process's in-memory creation set. The journal records the
   pass as outstanding until it completes, so a crash mid-pass re-enters it
   from the top on the next attempt and the rename is never reached with an
   unsynced chain. The outstanding-pass record is consumed by the post-rename
   adoption path as well, not only by the renaming path: when a crash lands
   after the rename but before the snapshot directory is `fsync`'d, adoption
   completes the identical path-derived, top-down, idempotent pass —
   re-syncing the entries derived from the recorded final path, including the
   snapshot directory that now contains the already-renamed file — before it
   may insert the cache row or clear the intent, and it never repeats the
   rename, whose temp no longer exists. If that pass cannot complete,
   adoption creates no row and clears no intent, so the durability ordering is
   never abandoned by recovery.

   Windows is bounded differently, and the design states the bound instead
   of asserting a flush the platform does not document. Windows has no
   documented parent-directory `fsync` primitive for a normal user:
   `FlushFileBuffers` is documented as flushing the buffers of a specified
   file and requires the handle to hold `GENERIC_WRITE`, and the only
   documented flush wider than one file — `FlushFileBuffers` on a volume
   handle — requires administrative privileges, which this contract
   refuses to make a deployment prerequisite. No directory-entry
   durability is claimed from NTFS journaling either: the journal is the
   platform's crash-consistency machinery, not a barrier this contract can
   order against a database commit. What the platform does document is a
   barrier for the published file itself, and the engine uses exactly it:
   first `FlushFileBuffers` on the temp file — the engine owns that handle
   with `GENERIC_WRITE` — then `MoveFileEx` carrying both
   `MOVEFILE_REPLACE_EXISTING` and `MOVEFILE_WRITE_THROUGH`. The replace
   flag is documented only as replacement semantics; the write-through
   flag is the barrier — under it the function does not return until the
   file is actually moved on the disk, and a move performed as a copy and
   delete operation is flushed to disk before the function returns. If
   any call in that sequence fails, the publish fails with nothing
   published: the temp is left for cleanup and the pending intent
   resolves under the pre-rename rule of step 6.

   The residual Windows gap is named, not papered over: no documented
   normal-user primitive makes a newly created ancestor entry durable, so
   a power-loss crash can lose such an entry both before the commit — the
   intent is still pending, and the file-absent arm of
   [Publish intent and restart recovery](#publish-intent-and-restart-recovery)
   resolves it: clear the intent, continue per the journal, and rebuild
   missing directories under the ordinary creation rules — and after the
   commit, where the file-only Windows barrier does not reach. A
   committed row whose recorded path no longer resolves is not a
   half-promotion — its bytes were verified before the commit — but it is
   never served: the lookup rule and the startup reconciliation pass
   detect an unresolvable committed path, mark the row non-playable and
   recoverable, and a fresh download job may republish a new snapshot
   (the post-commit loss row of the crash-point matrix). On Unix this
   case cannot occur: the `fsync` chain of this step orders every
   ancestor entry before the rename, and the rename before the commit,
   so no committed row can reference a path whose ancestor chain was not
   durably published.
6. **Commit.** Only after a successful rename does the cache row exist.
   The row records the `MediaKey` → cache-path mapping, the engine-computed
   digest, the digest provenance used, and the licence label at commit.
   The commit clears the journaled publish intent. A terminal state
   reached before the rename clears it too — nothing was published. A
   terminal state reached after the rename never abandons the intent with
   the file still on disk; that case is governed by the publish-intent
   protocol below. What the preceding step guarantees at commit is
   platform-exact: on Unix, the whole ancestor chain is durable, so a
   committed row's path survives power loss; on Windows, the published
   file's own bytes and entry are durable under `MOVEFILE_WRITE_THROUGH`,
   while a newly created ancestor entry remains subject to the post-commit
   loss row of the crash-point matrix — detected and repaired, never
   served.

Failure at any step:

- Temp reservation (including missing-ancestor creation): `OfflineError::StorageUnavailable`;
  the previous temp is unlinked, no cache row created.
- Receive: `OfflineError::StorageUnavailable` if the filesystem refuses the
  temp or the append.
- Finalize `fsync`: `OfflineError::StorageUnavailable`.
- Verify: `OfflineError::IntegrityMismatch` on digest mismatch;
  `OfflineError::IntegrityUnverifiable` when no provenance tier can supply
  an expected digest; `OfflineError::QuotaExceeded` when the verified size
  exceeds the per-track cap. The temp file is unlinked; no cache row is
  created; nothing was ever renamed.
- Publish: `OfflineError::StorageUnavailable`. A failed rename leaves the
  temp in place for cleanup and the cache path untouched — nothing was
  published, so a terminal state clears the intent under the pre-rename
  rule of step 6.

A half-promoted cache row that points at a missing or partial file is a bug
that the contract forbids; downstream layers must never observe one. The
`tracks` row remains untouched until step 6 succeeds, and the lookup path
between admission and publish returns the live endpoint only. The one named
exception is not a half-promotion: on Windows, a post-commit loss of a newly
created ancestor entry (created at step 1; given no documented
normal-user durability barrier) can
leave a committed row whose verified
bytes are no longer reachable. That row is never served — the lookup rule
and the startup reconciliation pass detect the unresolvable path, mark the
row non-playable and recoverable, and a fresh download job may republish a
new snapshot per the post-commit loss row of the crash-point matrix.

### Publish intent and restart recovery

The six steps leave exactly one window that ordinary journal recovery
cannot see: the rename happened and the step-6 commit did not — whether
by a crash between the two, or by a post-rename terminal transition
interrupted before its cleanup unlink. The temp file no longer exists to
resume from, and the final file exists without a cache row. The contract
closes that window with a persisted publish intent, a verdict-first
terminal rule, and a crash-point matrix enumerating every kill point in
the window and its single recovery resolution:

1. **The intent precedes the rename.** The last journal record written
   before the rename is an `fsync`'d publish-intent record naming the
   snapshot-scoped final cache path and the verified digest. A crash after
   the intent but before the rename therefore leaves a durable, inspectable
   record that a publication was about to happen.
2. **The intent is cleared on completion — never abandoned.** A successful
   step-6 commit clears the intent with a further journal record. A
   terminal state reached before the rename — failure, cancellation,
   supersession — clears it as well: nothing was published, and the temp
   file is cleaned up by the job's ordinary exit rules. A terminal state
   reached after the rename may not clear the intent on its own; the
   published final file would outlive the only record that explains it.
   That transition is itself journalled, in verdict-first order: it
   first appends an `fsync`'d terminal-verdict record — naming the
   terminal state, its redacted reason, the published final path, and a
   durable delete intent for that path — and only then touches the
   filesystem. The unlink of the published file goes through the same
   validated cache-unlink path eviction uses, idempotently, and the
   intent-clear is recorded only after the unlink succeeds. A step-6
   commit that fails after the rename is the same post-rename failure
   under the same rule: the verdict is durable before any cleanup runs.
   The verdict record survives every later crash: it is the durable
   delete owner that startup recovery consults and preserves until the
   cleanup it owns completes. A crash before the verdict record is
   durable is the ordinary journal-admission window every state
   transition shares — no destructive step has run, so the job resolves
   by the same rules the crash-point matrix below assigns to a rename
   with no recorded verdict, and a licence or capability revocation
   stays independently visible to the adoption guard through the
   registry's own durable state.
3. **Startup resolves a pending intent by inspection.** A job whose journal
   ends in a publish intent is resolved by examining the intent's final
   path — but only a publication-eligible job may adopt, and a matching
   digest alone is never authorization. The resolution order is normative
   and fixed: committed-row recognition first, terminal-verdict
   consultation second, adoption gates third. Recognition precedes every
   destructive step, so a committed row is never resolved by unlinking
   its bytes.

   **Recognition first.** Before any verdict consultation, adoption gate,
   or cleanup, recovery checks for an already-committed row. If a cache
   row exists whose `(source_key, track_key, snapshot_key)` exactly
   matches the intent's identity, whose recorded cache path equals the
   intent's final path, and whose recorded digest equals the intent's
   verified digest, the step-6 commit already happened: the row stands on
   its durable evidence, and recovery only clears the pending intent.
   Recognition never consults the file to authorize a destructive step:
   when the recognized row's recorded path no longer resolves — the
   Windows post-commit loss case — the intent is still cleared (the
   commit demonstrably happened), and the row is routed through the
   post-commit loss reconciliation, which marks it non-playable and
   recoverable and lets a fresh download job republish a new snapshot;
   this recovery never unlinks such a row's recorded path, and an absent
   file backed by consistent durable evidence is this case, not the
   contradictory-evidence case below. Adoption
   gates are never re-run against a committed row — an epoch change or
   revocation that lands after the commit governs the row's future
   through its own retirement protocol (licence revocation retires the
   row and preserves the file per the Licensing rules; a
   capability-driven retirement of the bytes goes through the staged
   tombstone of [Eviction](#cancellation-quota-and-eviction)), never
   through this recovery's unlink. A well-formed journal cannot carry
   both a recognized committed row and a terminal-verdict record for the
   same intent: the verdict-first protocol of rule 2 writes a verdict
   only before the step-6 commit and never after it, and a post-commit
   retirement runs through its own durable transitions without writing
   this protocol's verdict. If contradictory evidence of both appears
   anyway, recovery fails closed: the job is resolved as a terminal
   failure in the ledger without any destructive step, the row and the
   file are left untouched, and the durable evidence stays in place for
   adjudication — destructively resolving the contradiction is forbidden.
   A row found at the intent's identity whose key, path, or digest
   evidence is inconsistent is the same fail-closed case, for the same
   reason.

   **Verdict second.** Only when recognition has not resolved the intent
   does recovery consult the terminal-verdict record: a journal that
   carries a post-rename verdict — `Failed`, `Cancelled`, a superseding
   verdict, or a licence or capability revocation — together with a
   pending delete intent is never adopted as playable, whatever the
   surrounding job state claims. Recovery completes the interrupted
   cleanup that record owns — idempotent unlink of the published file if
   it survives, then the intent clear — and preserves the verdict until
   the intent-clear lands, with the terminal state standing. A crash
   before the verdict record is durable is the ordinary journal-admission
   window every state transition shares — no destructive step has run, so
   the job resolves by the same rules the crash-point matrix below
   assigns to a rename with no recorded verdict, and a licence or
   capability revocation stays independently visible to the adoption
   guard through the registry's own durable state.

   **Adoption third.** Adoption completes step 6 only for a
   publication-eligible job whose current authority is established at
   restart: a matching durable `SourceIncarnationId` plus the absence of a
   persisted revocation record is necessary but never sufficient. Every
   gate holds: no terminal verdict is recorded; the journaled state still
   permits publication (`Committing`); the job's recorded
   `SourceIncarnationId` still matches the source's current durable
   incarnation at restart — a replaced incarnation, or a source whose
   identity can no longer be rebound, retires the job and never adopts it,
   while a mere process restart that changed the transient generation
   number is rebindable and does not fail this gate; no licence or
   capability revocation is visible in the registry's durable state; and
   adoption, like a resumed job, establishes the source's current accepted
   generation and revalidates against that fresh generation the media
   identity, the `offline_snapshot` capability, the current
   `OperationalLicence`, and the captured `resume_validator`, exactly as
   the restart-authorization rule above does for a resumed job. A
   revocation or denial observable only through the live backend — a
   source that has since withdrawn the capability or revoked the licence
   without writing a durable revocation record — fails the gate: the
   absence of a persisted revocation is never a substitute for current
   authority. A job failing a definitive gate resolves as recorded-file
   cleanup — idempotent unlink of the published file, then the intent
   clear, with the terminal state standing. A job whose current authority
   cannot be established because the source is transiently unreachable or
   mid-reauthentication neither adopts nor destroys the publication: no
   row is created, no unlink runs, the pending intent remains the durable
   recovery/cleanup owner, and a later recovery pass retries once the
   current accepted generation is establishable — the same fail-closed
   retention the barrier arm below uses. That cleanup is reachable only
   when no committed row was recognized and no verdict owns the file. For
   an eligible job:
   - The file is present and its SHA-256 matches the journaled digest: the
     rename happened. Adoption must first complete and verify every pending
     platform publication barrier the crash interrupted — the outstanding
     durability pass of step 5, consumed here from the outstanding-pass
     record rather than re-derived from a temp file. On Unix that means
     re-deriving the complete ancestor chain from the recorded final path
     and re-syncing every entry top-down and idempotently, including the
     snapshot directory's own entry for the already-renamed file, and it
     never repeats the rename, because the temp is gone and the published
     name already exists. On Windows the published file's own bytes and
     entry barrier completed inside the rename call, so adoption adds no
     undocumented flush and the named ancestor-entry residual remains
     bounded by the post-commit loss row. Only once every applicable
     barrier holds does the engine complete step 6 — inserting the cache
     row; the `(source_key, track_key, snapshot_key)` key makes the insert
     idempotent — and clear the intent. If a required barrier cannot be
     completed, adoption creates no row and clears no intent: the pending
     intent remains the durable recovery/cleanup owner and a later recovery
     pass retries, so a committed Unix row never references a path whose
     rename entry is not durable.
   - The file is present and the digest does not match: the bytes are not
     the verified publication. The engine unlinks the file, clears the
     intent, and restarts the job from zero.
   - The file is absent: the rename never happened — or the published name
     was lost in a power-loss crash that took a not-yet-durable ancestor
     entry with it (on Unix, step 5's chain-durability ordering keeps that
     case below the commit; on Windows the same crash before the commit
     resolves here, and the same loss after the commit resolves through
     the post-commit loss row of the crash-point matrix). The engine
     clears the intent and continues per the journal — resume or restart
     from zero under the normal resumption rules.
4. **The gap is invisible downstream.** Between the rename and the commit
   or adoption there is no cache row, so lookups return the live endpoint
   exactly as before admission. The transient unowned file is observable
   only by the engine's own recovery pass, is bounded by the same
   per-track cap as any committed snapshot — the cap is enforced before
   the rename, so this window can never hold an over-cap file — and its
   lifetime ends at the
   post-rename terminal transition or the first recovery pass, whichever
   comes first.
5. **The crash-point matrix.** Every kill point in the publish window has
   exactly one durable observable state and one recovery resolution. The
   matrix is normative: an implementation may not introduce a resolution
   that is not a row of this table, and the verdict-first ordering of
   rule 2 is what keeps the last three rows unreachable by any
   destructive step.

   | Crash point | Durable state at restart | Startup resolution | Invariant |
   | --- | --- | --- | --- |
   | Ancestors created (step 1 or an earlier attempt), process crash before the step-5 ancestor-durability pass | No commit; temp present; final absent; ancestor directories may exist but their entry durability is unproven; no intent, or a pending one. | Ordinary journal recovery resumes or restarts per the journal; on reaching step 5 it re-derives the complete ancestor chain from the recorded final path and re-runs the top-down, idempotent pass before the rename, never trusting the crashed process's creation set. | The pass is path-derived and idempotent, so a resumed process completes it without the creator's memory. |
   | Power loss before or during the step-5 ancestor-durability pass, before the rename | No commit; final absent; the temp or some created ancestors may or may not have survived; a pending intent may be present. | File-absent resolution for any pending intent; on the next attempt the complete chain pass runs from the top before the rename. | Nothing was published; re-syncing the chain is unconditional and cannot double-publish. |
   | Power loss after the rename, before the snapshot-directory `fsync` and the commit | Intent present; the published name may or may not have survived, because the rename's own entry was not yet synced; the ancestor chain is already durable from the pass. | Final present with the intent's digest → the adoption path first completes the interrupted publication barrier — on Unix re-deriving the recorded final path's complete ancestor chain and `fsync`-ing the snapshot directory that holds the already-renamed file, without repeating the rename — and only then inserts the row and clears the intent; if that barrier fails, no row is created and the intent stays as the durable owner. Final absent → clear the intent and resume or restart per the journal. | Ancestors are durable before the rename, so only the rename's own entry is at stake, and either arm resolves without an orphan beyond one pass; no committed Unix row exists until that entry is durable. |
   | Process crash after the rename, before the snapshot-directory `fsync` and the commit | Intent present; the published name resolves in the still-running kernel and matches the intent's digest; the rename's own entry is not yet durable; the ancestor chain is durable from the pass; job `Committing`. | Adoption first completes the pending publication barrier — on Unix re-derives the recorded final path's complete ancestor chain and `fsync`s the snapshot directory holding the already-renamed file, never repeating the rename — then inserts the row and clears the intent; if the barrier fails, no row is created and the intent remains the durable owner. Because the barrier precedes the row insert, the recovery's own commit is durable and a subsequent power loss cannot lose the name. | The step-5 ordering is completed by recovery: a committed Unix row never exists before its publish-directory entry is durable. |
   | Verify passed, publish-intent record not yet durable | No intent; temp present; final absent; job pre-`Committing`. | Ordinary journal recovery; the job proceeds from its journaled state. | Nothing was published; no recovery protocol engages. |
   | Intent `fsync`'d, before the rename | Intent present; temp present; final absent; job `Committing`. | File absent → clear the intent; resume or restart per the journal. | No publish without a commit; temp resume intact. |
   | Rename applied, before the step-6 commit | Intent present; final present with the intent's digest; temp gone; job `Committing`; no verdict record. | Adoption path: digest match → complete the pending publication barrier first (on Unix re-derive the recorded final path's chain and `fsync` the snapshot directory of the already-renamed file; never repeat the rename), then the idempotent row insert completes step 6 and the intent is cleared; a barrier failure creates no row and retains the intent. | A row exists only after a verified rename and a completed durable publish barrier; no orphan beyond one pass. |
   | Rename applied, then a newly created ancestor entry lost for want of durability | Intent present; final absent (the published name did not survive); job `Committing`. | File-absent resolution: clear the intent; the job continues per the journal — resume or restart from zero; missing ancestor directories are rebuilt under the ordinary creation rules. | On Unix, step 5's chain-durability ordering confines this row below the commit. On Windows the same crash before the commit resolves through this row; the same loss after the commit resolves through the post-commit loss row below. |
   | Row committed (Windows), then power loss loses a newly created ancestor entry or the published name | Intent cleared at the commit; row present; the recorded path does not resolve. | Post-commit loss path: the row is never served — lookup and the startup reconciliation pass mark it non-playable and recoverable, a fresh download job may republish a new snapshot, and the never-a-silent-pass rule applies. The predecessor snapshot, if any, is untouched. | Windows documents no normal-user ancestor-entry durability barrier; detection and repair, not an undocumented flush, bound this case. Unix excludes it by the step-5 `fsync` chain. |
   | Crash during the step-6 commit (row inserted, intent not yet cleared) | Intent present; row present; final present. | Row-recognition path: the exact committed row — identity, recorded path, and recorded digest all matching the intent — stands; preserve its bytes, clear the intent. The insert an adoption would re-run is an idempotent no-op. | Completion is idempotent; a committed row is never re-adjudicated by adoption gates. |
   | Row committed (Windows), intent not yet cleared, and the recorded path lost to post-commit ancestor loss | Intent present; row present; the recorded path does not resolve. | Row-recognition path: the committed row stands on its durable evidence — identity, recorded path, recorded digest — and the intent is cleared; the unresolvable path routes the row through the post-commit loss reconciliation (never served, marked non-playable and recoverable, a fresh download job may republish a new snapshot), never through this recovery's unlink. | Recognition never consults the file to authorize cleanup; a committed row is never unlinked by publish-intent recovery, whatever the file's state. |
   | Row committed, then the source is replaced by a different durable incarnation before recovery | Intent present; row present; final present; the job's recorded `SourceIncarnationId` no longer matches the restored source's. | Row-recognition path: the committed row stands and the intent is cleared; the incarnation mismatch retires the job, never the row. Any later retirement of the row goes through its own retirement protocol, not the recovery unlink. | A committed row is never unlinked by publish-intent recovery; incarnation replacement governs admission and job adoption, not already-committed bytes. |
   | Row committed, then a licence or capability revocation lands before recovery | Intent present; row present; final present; the registry's durable state shows the revocation. | Row-recognition path: the row stands and the intent is cleared; the revocation is observed by reconciliation — licence revocation retires the row and preserves the file per the Licensing rules, and a capability-driven retirement of the bytes goes through the staged tombstone of [Eviction](#cancellation-quota-and-eviction). | Recovery never orphans a playable row; revocation retires rows through their own durable transitions, never the recovery unlink. |
   | Post-rename terminal decided, verdict record not yet `fsync`'d | Indistinguishable from the rename-applied row — no durable verdict exists. | The rule-3 resolution order: committed-row recognition first — a committed row stands and the intent is cleared — then the ordinary adoption gates, which refuse adoption for a revocation, a denied or withdrawn capability, or a replaced incarnation: the durable licence/capability state and durable `SourceIncarnationId` checks, plus the current-authority revalidation of capability, licence, and validator against the source's accepted generation. | The journal-admission window every state transition shares; verdict-first ordering means no destructive step has run. |
   | Verdict record `fsync`'d, before the unlink | Intent present; verdict present; final present. | Cleanup path: never adopt; idempotent unlink; then intent clear. | The verdict is durable before any destructive step. |
   | Unlink done, intent-clear not yet recorded | Intent present; verdict present; final absent. | Cleanup path: never adopt; clear the intent. | The verdict outlives the file; no resurrection. |
   | Pre-rename terminal decided, intent-clear not yet recorded | Intent present; temp present; final absent. | File absent → clear the intent; continue per the journal; the temp is cleaned by the ordinary exit rules. | The decision had not reached the journal; no destructive effect preceded its durability. |

Restart recovery and the staged delete of
[Eviction](#cancellation-quota-and-eviction) are the two crash-recovery
protocols of the cache engine; together they ensure no playable row is
ever served without its bytes, and no engine-owned file is ever stranded
without a row beyond one recovery pass. The publish-window rows of the
crash-point matrix are absolute on every platform: Unix re-derives and
chains directory `fsync`s from every ancestor entry through the rename to
the commit on every attempt — including a post-rename adoption, which
completes the same barrier before it may insert the row — so a resumed
process completes the ordering the crashed process began, and
Windows orders the published file's own bytes and entry through the
documented `MOVEFILE_WRITE_THROUGH` barrier before the commit. The one
platform-shaped residual — a Windows post-commit loss of a newly created
ancestor entry — is carried by the post-commit loss row of the matrix,
whose detection-and-repair resolution keeps such a row non-playable and
recoverable instead of claiming a flush the platform does not document.

### Per-source layout

The cache is split by exact `SourceId`, never by backend string or base URL:

- `<cache_root>/<source_key>/<track_key>/<snapshot_key>/`

`source_key`, `track_key`, and `snapshot_key` are **derived cache keys**,
not the raw identifiers: each is the first 32 hex characters (128 bits) of
`SHA-256(identifier_bytes)`. The identifiers are fed to the hash as their
exact, unmodified byte sequences — the engine still never parses,
normalises, or interprets them. The result is bounded (fixed length),
fixed-charset (`[0-9a-f]`), free of path separators, incapable of `..`
traversal, stable across runtimes, and reveals nothing about the identifier
it was derived from. Raw `TrackId` bytes — which may contain `/`, `..`,
unicode, or control characters — never appear in a path.

`source_key` and `track_key` are derived from the `SourceId` and `TrackId`.
`snapshot_key` is engine-minted at admission from the durable job ID
through the same first-32-hex SHA-256 discipline. Each committed snapshot
owns its own `<snapshot_key>/` directory, and one job publishes at most one
snapshot — so a refresh, which is always a new job, publishes beside its
predecessor instead of over it. The predecessor's row and bytes remain
valid and playable until the staged delete of
[Reconciliation](#reconciliation) retires them, which is what makes the
sibling rule of snapshot immutability implementable on the filesystem.

The durable mapping recorded in the cache row at commit is
`(source_key, track_key, snapshot_key)` → cache path; lookups are
table-driven. No code path reconstructs a cache path from an identifier
except through this recorded mapping, and no URL or credential is
recoverable from a location.

The file name inside `<snapshot_key>/` is an implementation-chosen,
credential-free constant — the directory is the per-snapshot scope, so the
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
| Subsonic | `GET .../download?view=...&id=<trackId>` authenticated through the exact-origin proxy. | Per-source byte total bounded at the source-adapter-declared cap; offline rows are still capped by the per-track cap. | None advertised by the API — double-fetch verification. | Bearer URL handling per `task-remediation-2026-07.md` P1.6 — only the proxy ticket ever reaches GTK. |
| Jellyfin | `GET /Items/<id>/Download` authenticated through the exact-origin proxy. | Identical. | None guaranteed by the API — double-fetch verification. | Same. |
| Plex | `GET /library/parts/<partId>` authenticated through the exact-origin proxy; uses `X-Plex-Token` only inside the proxy boundary. | Identical. | None advertised by the API — double-fetch verification. | Same. |
| DAAP | `DAAP.song` request, authenticated through the DAAP protocol-specific lane already retired to the source lifecycle. | Identical. | None advertised by the protocol — double-fetch verification. | DAAP connection still has exactly-once logout; committed cache rows survive disconnect — logout revokes only the in-flight lease. |
| Radio-Browser | Disallowed. | — | — | Streams are public and not licensable for offline by default; deny hard — `offline_snapshot` returns `Err(Denied)` (explicit opt-out, distinct from `None`/undeclared). |
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
   predecessor is queued for unlink and retired through the same staged
   delete as eviction — durable tombstone first, idempotent post-commit
   unlink, recovery pass for interrupted deletes — through the same
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

1. **Quota is global, and the accounting covers every byte the engine
   owns.** The application has one offline quota expressed in
   bytes. Sub-limits per source are advisory only at admission time. The
   charged total is the cache root's engine-owned footprint, not just
   committed rows: committed snapshot bytes, in-flight temp files
   (`<final_name>.part-<job-id>`), and published-but-uncommitted files
   held by a pending publish intent all count, because each consumes real
   disk whether or not its row exists. In addition to the global quota,
   each snapshot is bounded by a **per-track byte cap** that is enforced
   **before the step-5 rename**, so a published-but-uncommitted file can
   never exceed it:
   - At admission, a job whose declared total (`Content-Length` when
     known) exceeds the cap fails `QuotaExceeded` before any network work.
   - A response of unknown length is charged against the cap as it
     streams: as each journaled segment becomes durable, the job adds its
     bytes to both the track's running total and the global charged total
     and fails `QuotaExceeded` as soon as either bound is crossed — the
     cap for the track, the free quota for the global total — so an
     unknown-length download cannot stream past the cap.
   - Independently of any streaming estimate, the verified size on the
     temp file is checked against the cap before the rename (step 4/5). A
     snapshot whose verified size exceeds the cap resolves like any quota
     failure — the temp is cleaned per its state, no rename occurs, and no
     published-but-uncommitted file is created. Checking only at step 6
     would permit an oversized published-but-uncommitted file inside the
     rename-to-commit window; the pre-rename check is what keeps that
     window bounded by the per-track cap, as rule 4 of the publish-intent
     protocol assumes.
2. **Eviction is newest-first within source, oldest-first across sources.**
   When the quota is exceeded, eviction walks sources in oldest-cache-first
   order and within a source newest-first.
3. **Eviction is a staged delete.** Eviction first commits a durable
   tombstone: the row leaves the playable set in the transaction that
   marks it `Deleted`, and that transaction is the only thing the word
   "same transaction" ever promises. The unlink of the recorded file
   happens after that commit and is idempotent — unlinking an
   already-missing file succeeds. A crash between tombstone and unlink
   strands at most an owned, non-playable file until the recovery pass
   re-attempts the unlink for tombstoned rows that still record a path;
   no playable row is ever left without its bytes.
4. **Cancellation is local-failure equivalent.** A cancelled job leaves the
   same atomicity footprint as a `QuotaExceeded` failure: temp file
   unlinked, no cache row created. A cancel that lands in the
   rename-to-commit window is a post-rename terminal state under the
   publish-intent protocol: the `Cancelled` verdict — with its durable
   delete intent — is journalled and `fsync`'d first, the published file
   is unlinked through the validated cache-unlink path second, and the
   intent-clear is recorded last. A crash anywhere in that sequence
   leaves the verdict record as the durable delete owner that startup
   recovery consults to finish the cleanup. A cancelled job is never
   adopted as a playable row.

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
| Quota exceeded before publish (declared total, unknown-length streaming, or verified size) | Job fails terminally with `QuotaExceeded`; temp cleaned, no rename, no row, and no published-but-uncommitted file. |
| Filesystem refuses temp reservation | `Failed(StorageUnavailable)`. |
| User cancels a download | `Cancelled`. Temp unlinked. |
| Two requests for the same `MediaKey` race | Newest waits for terminal state of predecessor; admission is one-at-a-time. |
| Network dies between two byte ranges | Resumable; the resumed range request revalidates the entity with `If-Range` and continues from the journaled offset. A `200`/`412` answer discards partial bytes and restarts from zero. |
| Process restart with a changed transient generation on the same durable incarnation | Rebindable: the job reacquires a process-local lease and revalidates identity, capability, licence, and validator against the current accepted generation. Numeric generation change is expected and is not a rejection. |
| Process restart against a replaced durable incarnation, even with a recycled equal transient generation | Not authorized: the job resolves terminally and never resumes. Numeric equality of `capability_epoch` is never authorization; only a matching durable `SourceIncarnationId` is. |
| Process crash before the step-5 ancestor-durability pass, then resume and publication | Recovery re-derives the complete ancestor chain from the recorded final path and re-runs the top-down, idempotent `fsync` pass before the rename; it never relies on the crashed process's record of which directories it created. |
| Power loss before or during the step-5 ancestor-durability pass | Nothing was published; on the next attempt the complete chain pass re-runs from the top before the rename. |
| Radio-Browser adapter receives an offline request | `Err(Denied)` from the capability; no network work. |
| Local file is requested for offline | `None` from the capability; no offline layer is created; the file is already local. |
| Crash between the publish rename and the row commit | Startup recovery resolves the journaled publish intent: adopt (complete the commit) only for a publication-eligible job, otherwise unlink. Never a playable row without verified bytes, never a stranded orphan beyond one recovery pass. |
| Committed row (Windows) loses its recorded path to a post-commit power loss | Never served: lookup and the startup reconciliation pass mark the row non-playable and recoverable, and a fresh download job may republish a new snapshot. Not a half-promotion — the bytes were verified before the commit; the platform lacks a documented ancestor-entry durability barrier (see [Atomic storage](#atomic-storage)). |
| Row committed (Windows), publish intent still pending, and the recorded path unresolvable | Recognition stands the row on its durable evidence and clears the intent; the unresolvable path routes the row through the post-commit loss reconciliation — never served, marked non-playable and recoverable — never through the publish-intent recovery's unlink. |
| Failure, cancellation, supersession, or a commit error lands after the publish rename | Post-rename terminal rule, verdict-first: the terminal verdict and its delete intent are journalled and `fsync`'d before any destructive step; the published file is then unlinked through the validated cache-unlink path; the intent is cleared last. A crash before the unlink leaves the verdict record in place as the durable delete owner; startup recovery consults it, never adopts the job as playable, and finishes the cleanup. |
| Licence or capability revoked between the rename and the commit, whether recorded durably or observable only by re-establishing current source authority | The job is not publication-eligible: the pending intent resolves to recorded-file cleanup, never adoption. No playable row. A current accepted generation that is merely unestablishable at recovery defers non-destructively — no row, no unlink, the pending intent retained for a later pass — rather than treating the publication as revoked. |
| Row committed, then the source is replaced by a different durable incarnation or a revocation lands before startup recovery | Row-recognition path: the committed row and its bytes stand and the intent is cleared; the revocation or incarnation mismatch is applied to the row by its own retirement protocol — licence revocation retires the row and preserves the file, capability-driven byte retirement uses the staged tombstone — never by the publish-intent recovery's unlink. |
| Crash between a delete tombstone and its unlink | The row stays non-playable throughout; the recovery pass re-attempts the idempotent unlink for tombstoned rows that still record a path. |

## Migration plan

The implementation of this contract adds **one** migration introducing two
tables and backfilling saved-source incarnations: the offline cache table,
the durable download-job table, and a durable `SourceIncarnationId` for
every pre-existing saved source. The exact schema, indexes, and triggers
are deliberately left for the implementation record. The migration:

1. Creates the cache table keyed by the derived cache key
   (`source_key`, `track_key`, `snapshot_key`) — an identity in its own
   right, **not** a strict foreign key on `tracks(id)`. A nullable advisory
   link to `tracks(id)` may exist for UI join convenience, but the cache
   row must remain valid when the track's catalogue row is absent,
   replaced by a refresh, or never materialised: a remote track's offline
   snapshot exists independent of any local `tracks` row.
2. Creates the download-job table carrying the full job model — including
   the journaled offset, the captured `resume_validator`, the pending
   publish intent, the terminal-verdict record with its delete intent,
   and the digest provenance in use — so that job state is
   durable across process restarts. No offline job is memory-only.
3. Persists no row that points at a missing or partial file. Promotion to a
   cached row is exactly the moment the step-6 commit (of Atomic storage)
   succeeds — or a pending publish intent passes every adoption gate at
   startup (no terminal verdict, `Committing`, a matching durable source
   incarnation, no durable revocation, and a current-authority revalidation
   of capability, licence, and validator against the source's accepted
   generation) and is idempotently adopted, which completes that same step.
   The verified rename (step 5) publishes the bytes only; it creates no row.
4. Is reversible in the same way as migration 13: any error restores the
   complete predecessor schema and data so the upgrade remains retryable.
5. Backfills a durable `SourceIncarnationId` for every pre-existing saved
   source that lacks one, under the first-load rule of
   [restart authorization](#authenticated-resumable-download-jobs): the
   value is assigned once, derived stably from the row's durable identity,
   and persisted durably in the same saved-source record, so the first
   restart after the upgrade preserves each source's incarnation and a
   resumed job still rebinds. The backfill is idempotent and re-mints
   nothing for a source that already holds an incarnation.

A successful migration raises the application schema version by exactly one.

## Validation strategy

Each slice lands with its own focused regression suite. The slices are:

| Slice | Coverage |
| --- | --- |
| Identity | Same `SourceId` + `TrackId` semantics as live; no second identity kind minted. Derived cache keys: fixed hex charset and width, no separators or traversal, byte-exact identifier input. |
| Capability | Default-deny behaviour for adapters that opt out; Subsonic/Jellyfin/Plex/DAAP opt in. |
| Resumable job | Bounded, `If-Range`-validated range requests; `200`/`412` restarts from zero; journal survives crash (offset truncation, last-segment digest re-check); segment bytes durable before journal progress, with a short-file or digest-mismatch recovery restarting from zero without trusting the offset; no-validator jobs restart only. |
| Restart authorization | Lease reacquisition, resumption, and publish-intent adoption all rebind by durable `SourceId` + `SourceIncarnationId`, never by a transient generation number, and both resumption and adoption revalidate current authority — capability, licence, and validator against the source's accepted generation — rather than trusting a durable identity and the absence of a persisted revocation. A valid restart on the same durable incarnation with a changed transient generation authorizes and resumes; a replaced incarnation, even one whose transient generation recycles the job's recorded `capability_epoch`, does not authorize and terminates; a backend-side revocation that left no durable record refuses adoption; an unestablishable current generation defers non-destructively. Pre-existing saved sources receive one stable, persisted incarnation at first load and keep it across restart (no re-mint per load, no spurious replacement). No lease handle or credential is persisted. |
| Atomic storage | Same-directory temp reservation (missing ancestors created at reservation); verify-before-publish ordering; same-filesystem rename; cross-filesystem publish refused. Unix validation lane: the complete ancestor chain is re-derived from the recorded final path and re-synced top-down, idempotently, before the rename on every attempt — including after a crash that happened before the previous process ran the pass — and the rename-to-commit chain is ordered behind it. Windows validation lane: the documented `FlushFileBuffers` + `MOVEFILE_WRITE_THROUGH` barrier on the published file, and missing-path recovery — never served, marked non-playable and recoverable, a fresh job may republish — including the combined pending-intent + committed-row + unresolvable-path case. Both lanes: publish-intent recovery across every kill point of the crash-point matrix, including the crash-before-directory-sync and power-loss rows, the verdict-first post-rename terminal transition, its adoption gates (including the current-authority revalidation of capability, licence, and validator against the source's accepted generation), and committed-row recognition with a revocation or incarnation replacement landing after the commit, before recovery. |
| Digest provenance | Advertised digest compared exactly; double-fetch fallback equality; no-tier backends fail `IntegrityUnverifiable` before publish. |
| Credential boundary | No credential in metadata, file name, sidecar, log, or GTK row. Isolation scope per `task-remediation-2026-07.md` P1.6; redaction mechanics per P1.4. |
| Redirect policy | Per `task-remediation-2026-07.md` P1.4 matrix; HTTPS-only, no `Referer`, no HTTPS→HTTP downgrade. |
| Licensing | Default-deny; revocation retires rows but preserves files. |
| Reconciliation | Refresh creates a sibling with its own snapshot-scoped path; no in-place mutation; staged-delete unlink with idempotent recovery. |
| Cancellation | Lifecycle supersession cancels in-flight jobs; a cancel landing in the rename-to-commit window follows the verdict-first post-rename terminal rule and is never adopted as playable. |
| Quota and eviction | Quota accounting covers committed bytes, in-flight temps, published-but-uncommitted pending-intent files, and unknown-length responses charged per durable segment. The per-track cap is enforced at admission (declared total), during streaming for unknown-length responses, and on the verified size before the rename, so unknown-length data crossing the cap fails before publication and no published-but-uncommitted file ever exceeds the cap. Eviction walks sources oldest-cache-first, newest-first within a source; staged tombstone-then-unlink; recovery completes interrupted deletes. |
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

Until an offline-capable source opts in for the first time, none of the
offline machinery is exercised at runtime. The migration itself is
unconditional — it creates the two offline tables, backfills saved-source
incarnations, and raises the schema version whether or not any source ever
opts in — so byte-level identity
with a pre-migration database is explicitly **not** a guarantee, and no
migration test may promise one. The compatibility guarantee is
behavioural: a database whose offline tables are empty behaves exactly
like a database without the offline subsystem — identical query results
at the application level, no files under the cache root, no offline
runtime path exercised. A source that opts out — or revokes an earlier
opt-in — returns `Err(Denied)`
from `offline_snapshot`; the source stays default-deny for offline exactly as
it was before it opted in.

When the project eventually retires the offline subsystem, retirement is an
ordered, two-phase operation: every on-disk media file is reconciled while
the cache rows that own it still exist, and only then is the metadata
dropped. The order is normative — the follower migration never destroys the
`MediaKey` → cache-path mapping while an owning file may still exist:

1. **Quiesce admission and drain jobs.** The offline capability is withdrawn
   first: no new download is admitted, and every in-flight job is driven to
   a terminal state under the same supervisor rules as any lifecycle
   supersession — a cancelled job unlinks its temp file, unlinks any file
   it published without a row under the post-rename terminal rule, and
   promotes no row.
2. **Reconcile every row that owns a file.** The engine walks the cache
   table and, for each row regardless of state — including rows retired as
   `Revoked`, whose files revocation deliberately preserved — retires the
   row through the same staged delete used by eviction: the tombstone
   commits first, then the recorded file is unlinked through the same
   validated cache-unlink path used by eviction and catalogue
   invalidation, idempotently, after the commit. Unlinking an
   already-missing file succeeds; the row is tombstoned all the same. A
   crash-orphaned file left by an interrupted unlink is absorbed by the
   bounded root sweep of step 3.
3. **Sweep the engine-owned cache root.** After the row walk, the engine
   unlinks every remaining file inside `<cache_root>` — crash-orphaned
   `<final_name>.part-<job-id>` temps among them — and removes the
   now-empty `<source_key>/<track_key>/<snapshot_key>/` directories it
   created. Nothing outside the cache root is touched.
4. **Drop the metadata.** Only when no owning row remains does the follower
   migration drop the offline tables, indexes, and triggers in one
   transaction, mirroring the forward migration's reversibility: any error
   restores the predecessor schema and data so the retirement remains
   retryable.

Because a file is only ever unlinked while the row naming it is queryable —
as a live row or as a durable tombstone — or inside the bounded root sweep,
retirement cannot strand media that no remaining metadata can attribute, and
it cannot delete anything the offline subsystem does not own. No live
production path depends on the offline machinery existing.

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
  source identity, retirement, and redaction policy. This contract's durable
  `SourceIncarnationId` is a non-secret registry-side field that extends that
  document's saved-source record; the minting and re-minting rule above is
  filed there as a follow-up ADR.
- [`lastfm-scrobbling.md`](lastfm-scrobbling.md) — credential-free delivery
  and redaction policy precedent; the headless application owner composed
  in [#165](https://github.com/jm2/tributary/pull/165) is the reference
  shape for the offline-job supervisor.
