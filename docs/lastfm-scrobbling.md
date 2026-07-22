# Last.fm scrobbling contract

- Status: accepted P2.1 design; internal protocol/desktop-authorization/vault/queue/playback-evidence,
  delivery/lifecycle, and now-playing runtime implemented; product integration pending
- Decision date: 2026-07-20
- Implementation status date: 2026-07-22
- Tracking issue: [#50](https://github.com/jm2/tributary/issues/50)
- Playback evidence foundation: [`playback-history.md`](playback-history.md)

This document defines when Tributary may send listening metadata to Last.fm, how account authority
is retained, and how qualified scrobbles survive offline operation. The feature is deliberately
opt-in and fail-closed: loading a URI, selecting a row, or receiving unowned output progress is not
permission to disclose listening activity.

## Dated implementation boundary

The following inventory describes the internal implementation as of 2026-07-22. The rest of this
document remains the accepted normative contract for the complete feature; present-tense contract
language does not mean that every required product layer or acceptance case has shipped.

The implemented internal foundation includes:

- the bounded signed HTTPS client, strict response parsing, versioned native-vault record, one-way
  account binding, migration 17's atomic account-bound 10,000-row FIFO, and migration 18's exact
  binding-only fixed-category durable delivery/credential-cleanup singleton. Authentication
  envelopes are borrowed from zeroizing response storage, validate the complete JSON
  string/escape/surrogate grammar, decode tokens and returned credentials directly into zeroizing
  allocations, and preserve fixed provider/HTTP classification without generic secret-bearing
  JSON values or ignored-value scratch storage;
- a bounded GTK-free latest-only desktop-authorization owner. A request token remains solely inside
  its serialized owner, expires exactly 60 monotonic minutes after response observation, and keeps
  the token-bearing browser URL inside exact current owner authority. The opaque challenge exposes
  no production URL accessor or browser handoff. Begin, Finish, cancel, expiry, failure, and close
  revoke the internal URL allocation; Finish consumes an opaque one-shot seal and the token before
  exchange. Ordinary supersession and shutdown cancel and join child work, abnormal owner loss
  closes ingress with a fixed terminal status, and success returns only a move-only staged
  username/session-key grant. This core mints no UUID, writes no vault record, opens no browser, and
  has no production factory. Product integration must add a concrete consent-gated browser-launch
  operation and must not describe its unavoidable external URL handoff as synchronously revocable;
- an explicit `LastFmRuntimeActivation` capability intended for a future issuer that has first
  established consent and build enablement. No production application path issues that capability
  yet;
- runtime-only account attachment: playback-facing admission accepts a validated unbound scrobble,
  and the runtime attaches the current account's vault-derived binding at its ingress gate before
  sending the bound command to the serialized owner, including during that exact account's
  reauthorization;
- a standalone, policy- and network-free playback-occurrence state machine that freezes validated
  structured metadata, owns a random RFC 4122 version-4 occurrence UUID, and captures exactly one
  whole-second UTC start time from the first current-generation `Playing` or accepted-position
  proof. The first position anchors without earning credit; only strictly observed forward deltas
  qualify once at `min(ceil(duration / 2), 240 seconds)`. Pause, buffering, seeks, restarts,
  retries, duplicate/regressed/stale events, wall time, and natural end cannot fabricate credit.
  Retry retains occurrence identity, credit, timestamp, and one-shot latches while re-anchoring the
  new generation; terminal retirement is explicit. The authority is deliberately uncloneable and
  its diagnostics redact metadata, duration, timestamp, UUID, and generation;
- a serialized actor with bounded admission for 64 ordinary metadata commands and four reserved
  control slots: one delivery result, two lifecycle markers, and one explicit now-playing clear.
  Delivery, lifecycle, and playback retirement therefore cannot be starved by the ordinary FIFO;
- a runtime-owned, validated, uncloneable, account-independent `LastFmNowPlaying` ingress. The
  ingress gate attaches the exact current account and epoch, allocates a monotonic latest-only
  generation, and synchronously cancels its predecessor before the successor enters the bounded
  FIFO. Explicit clear uses its reserved slot, advances ingress ownership, cancels synchronously,
  and makes the actor join the predecessor before acknowledging the clear. Now-playing is never
  persisted or retried and has fixed accepted, ignored, rejected, unavailable, incompatible, and
  capability-unavailable outcomes. Those outcomes cannot mutate durable delivery; only provider
  code 9 may atomically claim the exact current account, epoch, and now-playing generation, commit
  the durable reauthorization pause, and then retire delivery. Normal lifecycle paths and
  supervised owner failure or caught panic cancel and join the task before releasing authority. A
  hard external owner abort instead marks the drain barrier `Failed`; owner drop cancels the child
  before its primary lease share is released, and the request future's child-held shared vault
  lease excludes any successor until that future is actually dropped. This request-scoped proof
  does not turn hard abort into a joined drain for an independently active durable-delivery worker;
- one oldest-first delivery worker, batches of at most 50 rows, and at most one request in flight.
  The worker prepares and submits data but cannot mutate SQLite; the actor owns exact-receipt
  terminal settlement, durable rescheduling, and bounded accepted/ignored/rejected counters;
- a closed delivery classification: only timeout, transport, provider codes 8/11/16/29, and HTTP
  temporary-service/rate-limit failures retry, using durable 30-second exponential backoff capped
  at one hour. Accepted,
  ignored, and recognized terminal service results settle; incompatible HTTP/body/response results
  retain and quarantine the exact batch; and code 9 retains the queue for reauthorization. When
  SQLite accepts a pause, its commit precedes worker stop, survives restart without spawning a
  worker, and clears only through exact reauthorization or an opaque category- and runtime-bound
  explicit recovery command. If persistence fails, the actor closes admission, reports a fixed
  storage/capability failure, and stops the worker without claiming a restart-stable pause;
- a same-account live-reauthorization handoff that preserves the opaque account binding, admits one
  secret-bearing transition through completion, atomically excludes disconnect while it owns the
  transition, keeps queue admission open for that same binding while network delivery is stopped,
  and cannot restart delivery or publish `Active` after shutdown closes admission;
- checked delivery generations and stale-result retirement, including retention and replay when a
  request may have been accepted remotely before local terminal deletion. This is deliberately
  at-least-once delivery, not an exactly-once claim;
- lifecycle-owned disconnect, shutdown, and vault recovery: disconnect retires delivery,
  atomically replaces the purged queue with a cleanup tombstone, and clears that marker only after
  exact credential deletion; failed vault or marker cleanup is restart-stable and retryable,
  shutdown closes admission and drains admitted durable work while cancelling and joining network
  work, and runtime startup plus explicit missing/corrupt-vault recovery share a process-wide vault
  lease so successor ownership cannot overlap blocking vault operations or destructive recovery;
  and
- process-wide panic reporting that emits fixed diagnostics and never renders a panic payload,
  including payloads from caught worker, actor, or blocking-operation panics. Actor unwind is
  caught while its complete owner state and vault lease remain retained; ingress closes, the
  worker/relay are cancelled and joined, then the owner attempts to commit or validate a durable
  capability pause for any still-unpurged account before releasing the lease. If SQLite cannot
  establish that pause, the shutdown proof remains failed and no durable-pause claim is made.

This foundation is intentionally not exposed as a partial user feature. Still remaining are the
production playback owner that creates the occurrence state only after exact source/session
eligibility, converts immutable structured `Track` metadata, dispatches its now-playing/scrobble
actions, and issues explicit clear without crossing GTK borrow boundaries; localized consent and
browser invocation around the completed authorization core; one process-wide production owner;
atomic staged-session vault installation, exact same-account reauthorization and different-account
replacement/purge policy; enablement, exact per-source/session policy, and a production activation
issuer; application startup/shutdown
ownership; settings, account/recovery/status, valid-vault corrupt-queue recovery, accessibility,
and all localization UI; release-time production credential injection and package verification;
and the remaining end-to-end and platform acceptance matrix. The internal observer and
now-playing lane are complete but deliberately unwired; the countable P2.1 record stays open until
the product layers land.

The central rule is:

> Tributary submits only a bounded snapshot of structured track metadata after current,
> generation-owned playback proves the listening event. User session authority stays in the
> operating-system credential vault, while an account-bound durable FIFO retains only scrobbles
> that have already crossed the accepted threshold.

## Official protocol basis

The implementation follows Last.fm's current official documentation:

- [desktop application authentication](https://www.last.fm/api/desktopauth), including the
  60-minute request-token lifetime, single-use session exchange, and revocable long-lived session
  key;
- [`auth.getToken`](https://www.last.fm/api/show/auth.getToken) and
  [`auth.getSession`](https://www.last.fm/api/show/auth.getSession);
- [Scrobbling 2.0](https://www.last.fm/api/scrobbling), including threshold, metadata, FIFO cache,
  batching, retry, ignored-result, and correction rules;
- [`track.updateNowPlaying`](https://www.last.fm/api/show/track.updateNowPlaying) and
  [`track.scrobble`](https://www.last.fm/api/show/track.scrobble);
- the [API error-code list](https://www.last.fm/api/errorcodes),
  [API terms](https://www.last.fm/api/tos), and
  [Last.fm privacy policy](https://www.last.fm/legal/privacy).

Last.fm's pages remain authoritative if the remote service changes. Such a change requires a
reviewed contract and compatibility update; it must not be silently inferred from an unexpected
response at runtime.

## Delivery boundary and non-goals

The accepted initial P2.1 scope requires one active Last.fm account, browser authorization, secure
session retention, now-playing notification, durable scrobbling, explicit source policy, localized
status and recovery, and bounded fake-service coverage. It does not include:

- Last.fm radio, playback, recommendations, loved-track mutation, tag mutation, profile browsing,
  or listening-history import;
- password collection or the mobile-session authentication flow;
- automatic metadata correction, MusicBrainz lookup, filename parsing, or fuzzy matching;
- editing or deleting a scrobble after Last.fm accepts it;
- multiple simultaneously connected Last.fm accounts;
- Radio-Browser station or stream scrobbling; or
- an exactly-once delivery claim. Last.fm exposes no client idempotency key for this operation.

The feature uses only HTTPS Last.fm endpoints. Redirects must remain within the exact approved
Last.fm HTTPS origin policy and must never forward a session key or signed request to another
origin or a downgraded connection.

## Distribution prerequisite and application credentials

Last.fm requires an application API key and shared secret. A production Tributary distribution
therefore enables this feature only when both values were injected at build time from the release
environment:

- neither value is committed to the repository, copied into runtime configuration, requested from
  the user, or accepted from a command-line argument;
- missing, empty, or malformed build values compile to an explicit **Last.fm unavailable in this
  build** capability rather than a partly working authorization button;
- tests use dedicated fake values against a local fake service and never the production key; and
- logs, diagnostics, panic output, `Debug` implementations, and user-visible errors expose neither
  value nor any signed parameter string.

The shared secret is necessarily present in a Last.fm-capable desktop binary because Last.fm's
signature protocol requires it. It is an application credential, not a substitute for protecting
the user's session key. Registration of a production API account, review of the current
[Last.fm API terms](https://www.last.fm/api/tos), and secure injection into every supported package
job are external release prerequisites. Until they are completed, ordinary development builds
must report the feature as unavailable rather than ship placeholder credentials.

## Explicit consent and privacy UX

Scrobbling is disabled on every existing and fresh profile. No authorization request, now-playing
request, durable queue insertion, or scrobble request occurs before the user explicitly enables
the integration and accepts a localized disclosure. The disclosure must say that:

- Tributary sends the track artist and title, and may send album, album artist, track number,
  duration, and the UTC time playback began;
- Last.fm associates those values with the authorized Last.fm account and may publish or use them
  for listening history, charts, and recommendations;
- qualified offline scrobbles are stored locally until delivered, disconnected, or explicitly
  purged;
- authenticated remote music servers can independently scrobble the same playback, so every
  remote catalogue source starts excluded; and
- output servers or receivers, especially MPD installations with a separate scrobbler, can also
  submit independently. Tributary cannot detect those external integrations, so remote-source
  exclusion reduces but cannot eliminate duplicate submissions.

Consent precedes the browser flow. Dismissing or cancelling the disclosure leaves the feature
disabled and stores nothing. The feature's enabled state and per-source choices are non-secret
preferences, but the connected username is loaded from the credential vault and is not duplicated
into plaintext configuration.

The settings surface shows the exact returned account name, whether scrobbling is active, whether
reauthorization or secure storage is required, the number of pending scrobbles, and the remote
source policy. It provides **Connect**, **Reauthorize**, and **Disconnect and purge** actions with
localized accessible names, descriptions, progress, success, and fixed-category failures. Internal
HTTP, XML/JSON, database, and credential-store details remain in sanitized diagnostics.

## Desktop authorization and account identity

Authorization is one cancellable latest-only desktop flow:

1. After consent, Tributary calls `auth.getToken` using the build-provided application credential.
2. It retains the returned request token only in memory, records its monotonic 60-minute deadline,
   and opens the system browser at Last.fm's HTTPS authorization page with the exact API key and
   token.
3. The settings UI asks the user to return after approving access. **Finish authorization** calls
   `auth.getSession` exactly once for that token. A second click, stale callback, superseded flow,
   cancellation, shutdown, or deadline expiry cannot replay it.
4. Only a successful, structurally valid response supplies the account username and session key.
   Tributary generates a new random opaque account UUID and atomically stores the three-field
   account record in the operating-system credential vault before enabling scrobbling.

As of 2026-07-22, the internal authorization core implements the bounded latest-only request flow
through the move-only staged username/session-key result in step 4. No production path constructs
that owner, records consent, launches the browser, creates an account UUID, installs the staged
grant in the vault, or applies same/different-account transition policy yet; those operations must
land as one fail-closed integration rather than exposing a partial feature.

Tributary never asks for a Last.fm password. The request token, session key, username, and opaque
account UUID have content-redacted error and debug representations. An authorization token is not
persisted, and failure to open the browser leaves a visible copy/open-again action while its
existing in-memory deadline remains authoritative.

One profile may have only one active account. Error code 9 pauses queue delivery and asks for
reauthorization. A replacement session may retain the existing account UUID and queued rows only
when `auth.getSession` returns the exact same username bytes. A different username is an account
replacement: the UI must explain that the old account's pending rows cannot be transferred and
must run the complete disconnect-and-purge path before installing a new UUID. Tributary never
guesses account equivalence through case folding, display text, or a Last.fm profile URL.

## Credential-vault contract

The session key, returned username, and random account UUID exist durably only as one record in the
platform's operating-system credential vault (Secret Service/libsecret, macOS Keychain, or Windows
Credential Manager through the selected maintained abstraction). There is no plaintext database,
configuration-file, environment-variable, command-line, log, or home-directory fallback.

Vault creation, startup lookup, or deletion failure disables request and queue admission
immediately and presents a fixed localized **secure storage unavailable** state. A transient vault
read failure does not silently purge a queue or create another account; delivery remains paused
until the exact record can be recovered. A missing record while preferences claim the feature is
enabled is the same fail-closed state. During exact same-account code-9 reauthorization, a failed
vault update retains the already-valid prior record and durable reauthentication marker: network
delivery stays stopped, but offline queue admission may remain open for that same binding. Corrupt
or oversized fields are rejected without including their contents in diagnostics.

When a missing or corrupt vault record cannot be recovered, the settings surface may offer an
explicit **Discard quarantined scrobbles** recovery action. It first closes occurrence and queue
admission, drains every queue write admitted before that close through the same FIFO database
barrier as normal disconnect, stops delivery, and prevents a successor account from being created;
it then deletes the closed queue snapshot before resetting the disabled preference. The purge is
bounded by the snapshot's maximum monotonic row identity, so any true successor row admitted only
after that snapshot cannot be selected. Vault failure never triggers this destructive path without
confirmation.

The durable queue binds to a one-way account-binding digest derived from the vault-only random UUID
with a fixed domain separator. The raw UUID and username are never copied into SQLite. A queue whose
binding does not exactly match the current vault record is quarantined: no row is sent, reassigned,
or silently deleted. Normal account replacement cannot create this state because replacement first
purges the old queue.

## Eligible sources and metadata

Eligibility is evaluated from the immutable `QueueItem`/`Track` snapshot owned by the genuine
playback occurrence, never from the currently selected GTK row after playback starts.

| Playback owner | Initial policy |
|---|---|
| Built-in local library | Eligible after the user enables Last.fm. |
| Retained removable-media track | Eligible after the user enables Last.fm. |
| Structured operating-system-opened file | Eligible after the user enables Last.fm. |
| Authenticated Subsonic, Jellyfin, Plex, or DAAP catalogue | Disabled per source by default; the user must explicitly opt in after the duplicate-scrobble warning. |
| Any occurrence projected through a regular or server-native playlist | Uses the occurrence's real source owner and that owner's policy; playlist membership never launders a remote row into the local policy. |
| Radio-Browser station/stream | Always excluded because Tributary lacks authoritative track boundaries and duration for the changing broadcast. |
| Unknown, unavailable, retired, or stale-session source | Excluded. |

A remote opt-in is bound to the exact saved `SourceId`. A different endpoint/source identity starts
off, even if its label or account name matches. Disconnecting a music source makes captured stale
occurrences ineligible; reconnecting does not transfer authority to a predecessor session. The
audio output (local, AirPlay 1, Chromecast, or MPD) does not change the media owner's source policy.

Only fields already present in the structured `Track` snapshot may cross the boundary:

- `artist_name` and `title` are required, must each contain non-whitespace text, and are limited to
  1,024 UTF-8 bytes; control characters are rejected;
- `album_title` and `album_artist_name` are optional and, when present as non-whitespace text, are
  each limited to 1,024 UTF-8 bytes and may not contain control characters;
- `track_number` is optional;
- `duration_secs` is required and must be greater than 30; and
- the scrobble timestamp is the occurrence's captured UTC start evidence, converted to whole Unix
  seconds.

Accepted strings are sent byte-for-byte. Tributary does not trim, normalize, case-fold, split an
artist field, parse a filename or URI, consult a server again, infer an album artist, or substitute
display fallbacks. An empty optional field is omitted. An oversized field, absent required field,
control-bearing field, non-positive/unrepresentable start time, missing duration, or duration of 30
seconds or less makes that occurrence ineligible for both now-playing and scrobbling. No partial or
truncated metadata is sent. `mbid`, `context`, `streamId`, and `chosenByUser` are omitted because the
current `Track` and playback model does not own authoritative values for them; Last.fm consequently
applies its documented default for `chosenByUser`.

## Playback occurrence and authoritative evidence

The Last.fm observer consumes the same generation-owned playback evidence and discontinuity rules
as [`playback-history.md`](playback-history.md), but it is not downstream of the local-only history
database command. This lets eligible non-local owners participate without writing their counts into
the local `tracks` table.

A Last.fm occurrence begins only after a media load for the current queue occurrence is accepted.
Navigation, explicit selection, end-of-stream advance, and Repeat One create new occurrences.
Pause/resume, buffering recovery, seek, the three-second Previous restart, and a delivery retry
remain the same occurrence. A rejected load, stale output generation, unavailable source, terminal
error, Stop, source retirement, queue replacement, or application restart cannot contribute old
progress to a successor.

The first authoritative playing evidence is either:

- a `Playing` state event owned by the occurrence's accepted current output generation; or
- for a backend that does not publish a clean `Playing` transition, the first current-generation
  position sample accepted by the same no-credit proof path used by playback history.

Buffering, Paused, Stopped, load acceptance alone, wall-clock delay, and stale-generation events are
not playing evidence. At the first evidence, Tributary captures one UTC start timestamp and closes
the occurrence's now-playing latch before scheduling network work. Pause/resume or a delivery retry
does not move that timestamp or reopen the latch.

Only observed strictly forward playback earns scrobble credit. Initial samples and all samples
after pause, buffering, resume, seek, Previous restart, retry, or another discontinuity re-anchor
without crediting the jump. Paused/buffering time, wall-clock time, duplicate/regressed samples,
forward seeks, and unobserved tails earn nothing. Listening again after a backward seek may earn new
credit, but each occurrence has a one-shot scrobble-admission latch.

## Now-playing behavior

For an eligible occurrence, Tributary attempts `track.updateNowPlaying` once, immediately after the
first authoritative playing evidence. The request uses the exact frozen metadata and current vault
session. It is cancellable, generation-owned, and bounded, but it is never persisted or retried.
Offline state, a timeout, network failure, service code 8/11/16/29, malformed response, shutdown, or
supersession simply ends that occurrence's now-playing attempt.

Error code 9 still pauses subsequent Last.fm work and exposes **reauthorization required**, but the
old now-playing request is not replayed after reauthorization. Accepted or ignored now-playing
responses do not alter Tributary metadata. Last.fm corrections are ignored and are never fed into a
later scrobble.

## Scrobble threshold and durable admission

An eligible known-duration occurrence qualifies after this amount of observed forward playback:

```text
min(ceil(duration_ms / 2), 240_000 ms)
```

The frozen structured `Track.duration_secs` supplies `duration_ms`. The boundary is inclusive: a
31-second track qualifies at 15,500 milliseconds; a 480-second or longer eligible track qualifies
at 240,000 milliseconds. Unknown-duration and at-most-30-second occurrences never qualify, even at
natural end or after four minutes. Natural end by itself grants no missing credit.

When the threshold is crossed, the occurrence closes its admission latch before synchronously
submitting one queue-insert command. Runtime ingress attaches the active account binding under its
gate, the serialized actor rechecks the exact active account and epoch, and the database transaction
enforces the single queue binding and global cap before committing. No scrobble network request may
include that row before the commit. A failed insert or full queue cannot be reconstructed from later
events because the occurrence latch stays closed; it produces a visible fixed-category failure
rather than risking duplicate admission.

The queue is capped at exactly 10,000 rows globally for the one active account. At the cap,
Tributary refuses every new qualified row all-or-none and never silently evicts an older scrobble.
Playback continues normally, and the settings status plus a rate-limited toast report that new
scrobbles are not being saved until pending rows are delivered or purged.

## Durable queue and privacy boundary

SQLite migration 17 adds a strictly recognized, account-bound FIFO. Each row persists only:

- an opaque row identity and monotonic FIFO ordering state;
- the one-way account-binding digest;
- required artist, title, UTC start timestamp, and known duration;
- optional album, album artist, and track number; and
- bounded retry state: saturated attempt count and next eligible attempt time.

It does not persist a Last.fm username, account UUID, session key, request token, API secret,
`SourceId`, native track ID, playlist identity, URI, file path, artwork, genre, rating, local play
count, raw response, or raw error. Queue models and errors expose only opaque identity, counts,
timestamps, and fixed categories through `Debug`.

FIFO order is admission order, not playback-start order: a long track can qualify after a shorter
track that started later. The oldest pending rows always block newer rows from being sent ahead.
Migration 18 upgrades an already-applied migration-17 database with one exact singleton delivery
gate. Its only fields are the singleton slot, the same one-way account-binding digest, and one fixed
numeric category: reauthentication, compatibility, capability, or credential cleanup required. It
contains no username, credential, listening metadata, response, endpoint, or diagnostic text.
Result-driven pause writes validate the exact receipt; worker-failure pauses validate the current
account. A successful transaction commits before a Stop acknowledgement or durable paused status
is published. If that write fails, the actor closes admission and stops delivery with a fixed
capability/storage failure, but does not describe the uncommitted state as restart-stable. Startup
reads the queue and marker coherently and restores a committed fixed phase without spawning a
delivery worker. The cleanup
category additionally requires an empty queue and opens only the cleanup-retry path: it never
retains a session in the actor, admits metadata, or starts network work. Code 9 can clear only after
exact same-account vault reauthorization; compatibility and capability markers require an opaque
explicit-recovery capability bound to that exact runtime, account epoch, watched pause revision,
and category. The cleanup marker is not manual-recovery authority. Stale receipts, generations,
accounts, revisions, recovery categories, or cleanup states change nothing.

Both migrations are transactional and idempotent and validate their already-present tables and
indexes/constraints. Closed missing/corrupt-vault recovery purges the queue and marker in one
transaction. Normal disconnect instead atomically purges the queue while replacing any delivery
pause with the cleanup marker; exact vault deletion and exact cleanup-marker deletion form a second,
retryable cross-store stage. Downgrade refuses while either a queue row or marker exists; an empty
state may be downgraded without leaving account or listening metadata behind.

The queue itself is private listening history. It uses the existing application data-file
permissions, is included in the consent disclosure, and is purged on explicit Last.fm disconnect.
It is not displayed as a browsable track-history feature or exported through playlist formats.

## Submission, retry, and response handling

Runtime startup coherently loads the durable account state and vault authority. An active or
delivery-paused account requires and retains the exactly matching session. A cleanup marker with a
still-present matching vault record instead creates a sessionless cleanup-only actor; if the vault
record is already absent, startup compare-and-deletes only the exact inspected cleanup marker and
reports typed cleanup completion without exposing an active handle. The runtime otherwise restores
a durable paused phase without a worker or gives one generation-owned worker the matching session.
The worker sends the oldest eligible rows to `track.scrobble` as an
HTTPS form POST. One request contains at most 50 rows, preserving FIFO order and Last.fm's indexed
parameter/signature rules. At most one batch is in flight, so a retrying head cannot be bypassed by
new work.

Every request has independent connection and operation deadlines plus a bounded response body.
HTTP status alone is never treated as success; the worker parses the complete Last.fm envelope and
maps each returned scrobble to its request position before changing durable state.

The result policy is closed and exhaustive:

- an explicitly accepted item is terminal-success and is deleted transactionally;
- an item carrying any nonzero `ignoredMessage` code, including an unknown future ignored code, is
  terminal-ignored and is deleted without automatic modification or resubmission;
- HTTP 429 or 5xx without a recognized provider error envelope is transient and retains the
  complete batch for retry; a recognized provider envelope retains its own closed classification;
- top-level service codes 8, 11, and 16 and rate-limit code 29 are transient and retain the complete
  batch for retry;
- DNS, connect, TLS, timeout, and response-body stream interruption retain the complete batch for
  retry; policy failures and a response exceeding the fixed body limit remain compatibility
  failures rather than transient transport outcomes;
- top-level code 9 retains the complete queue, closes network admission, and pauses delivery until
  the same account is successfully reauthorized;
- every other recognized top-level Last.fm error is terminal for that submitted batch and is not
  retried; and
- a fully received but structurally incoherent success response cannot safely prove which rows
  were accepted. It retains and quarantines the batch, pauses automatic delivery with a visible
  compatibility failure, and never guesses from aggregate accepted/ignored counts.

Accepted and ignored items are classified independently for bounded aggregate counters, but one
complete structurally valid terminal response settles its complete exact receipt atomically. The
durable mutation is committed before another batch starts. Terminal errors and ignored items update
only bounded aggregate status; raw response text and metadata are not copied into logs or a failure
ledger.

Reauthentication, compatibility, and capability pauses are durable delivery state, not retry
timers. Restart never clears them or starts a worker. An ordinary failed same-account vault save
retains the reauthentication marker and keeps same-binding queue admission open. Compatibility or
capability delivery resumes only after an explicit runtime/account/revision/category-bound recovery
request retires the old generation and atomically clears the matching marker; a failed transition
keeps or atomically replaces a durable closed category rather than exposing an unmarked restart
window. The credential-cleanup category is separately closed to all delivery and queue admission.
These internal authorities are not production-wired to UI yet.

Transient retries have no attempt limit. After each transient result, the saturated attempt count
and a clock-based not-before value are committed using a deterministic exponential schedule that
starts at 30 seconds and caps at one hour. Restart preserves that schedule; a successful batch
resets progression for the next head. Tests inject the clock and transport and do not depend on
sleeping or Internet access.

Delivery is at least once, not exactly once. A request can reach Last.fm and then lose its response,
or the process can stop after Last.fm accepts a batch but before SQLite commits deletion. Those rows
remain and may be submitted again. Tributary must prefer that bounded duplicate risk over silently
losing admitted offline history, and the privacy disclosure must not claim duplicate-free delivery.

Last.fm corrections returned by either endpoint are ignored. Tributary never rewrites queued rows,
library tags, playlist metadata, or a later occurrence from service-provided correction text, and
there is no automatic correction request.

## Disconnect, replacement, and shutdown

**Disconnect and purge** is destructive and requires confirmation when pending rows exist. It:

1. closes Last.fm occurrence, queue, and network admission and cancels current authorization and
   now-playing work;
2. retires the delivery generation so a late response cannot mutate successor state;
3. drains earlier admitted queue writes, then transactionally purges all Last.fm queue rows and
   account-scoped retry state while installing the exact binding-only credential-cleanup marker;
4. wipes the retained in-memory session and deletes only the exact matching vault account record;
   and
5. compare-and-deletes the cleanup marker before reporting completion.

If vault deletion or cleanup-marker deletion fails, the queue remains purged, the durable cleanup
marker remains authoritative, and all Last.fm work remains disabled; the UI reports an incomplete
secure-store cleanup and offers a retry. Restart with a matching record restores only a sessionless
cleanup state and creates no delivery worker. If vault deletion committed immediately before a
crash, the exact inspected marker plus an absent vault record authorizes idempotent marker cleanup
and a typed completed outcome without an active handle; a different stored account never does.
Replacing an account uses this same path. Tributary cannot retract a
request Last.fm accepted before cancellation, so the confirmation explains that already-submitted
history remains on Last.fm.

Normal application shutdown first closes the shared playback/Last.fm admission gate. It drains all
queue INSERT, accepted/terminal DELETE, retry-state, and purge commands admitted before the FIFO
barrier, but it does not wait indefinitely for authorization, now-playing, or scrobble network I/O.
Network work is cancelled or allowed only its already bounded deadline; any row without a committed
terminal result stays queued for the next launch. A forced process termination can lose only
pre-threshold in-memory progress or a queue command not yet committed; it cannot justify sending an
uncommitted row.

## Required implementation and validation

The implementation is complete only when all of the following are covered:

- build-time missing/invalid/fake application credentials and no production secret in repository
  or test artifacts;
- latest-only browser authorization, 60-minute expiry, single-use exchange, cancellation,
  supersession, browser-open failure, same-account reauthorization, and different-account purge;
- vault create/read/update/delete failures on the supported platform abstraction, no plaintext
  fallback, redacted debug/log/panic behavior, mismatched binding quarantine, and startup recovery;
- exact source-policy coverage for local, removable, external, all four authenticated remote
  adapters, mixed regular/server-playlist occurrences, Radio-Browser, stale sessions, and remote
  source replacement;
- exact 1,024-byte metadata edges, Unicode, required/optional empty values, duration 30/31 seconds,
  absent duration, structured-field-only behavior, and omission of unsupported parameters;
- first authoritative playing evidence, no-credit anchors, stale/rejected generations, pause,
  buffering, seek directions, Previous restart, retry, Repeat One, output error, natural end,
  source retirement, and one-shot now-playing/scrobble latches;
- exact half-duration ceiling and four-minute cap edges based only on accumulated forward evidence;
- atomic admission-before-network, exact 10,000-row contention at the transaction boundary,
  fail-visible refusal without eviction, FIFO ordering, 50-item batch boundaries, and account
  isolation;
- accepted, independently ignored, 8, 11, 16, 29, 9, every other known error, unknown ignored code,
  non-200 success/failure envelopes, malformed complete responses, interrupted responses, timeout,
  offline/restart backoff, ambiguous accepted-before-delete replay, and correction neutrality;
- disconnect races, in-flight response retirement, purge-before-vault-delete failure, normal
  shutdown's database barrier, forced cancellation retention, migration validation/downgrade, and
  secure queue-model redaction; and
- accessible settings states and exact key/placeholder parity across all 13 shipped locale
  catalogs, with no substantive English fallback.

No CI test calls the public Last.fm service. Protocol tests use a bounded local loopback fake
service or a transport abstraction, while the release checklist separately verifies that a
production API account is registered, injected, and permitted under Last.fm's current terms.
