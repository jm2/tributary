# Last.fm scrobbling contract

- Status: accepted P2.1 design; protocol/vault/queue foundation implemented; playback/runtime/UI pending
- Decision date: 2026-07-20
- Tracking issue: [#50](https://github.com/jm2/tributary/issues/50)
- Playback evidence foundation: [`playback-history.md`](playback-history.md)

This document defines when Tributary may send listening metadata to Last.fm, how account authority
is retained, and how qualified scrobbles survive offline operation. The feature is deliberately
opt-in and fail-closed: loading a URI, selecting a row, or receiving unowned output progress is not
permission to disclose listening activity.

The implemented foundation includes the bounded signed HTTPS client, strict versioned native-vault
record and one-way account binding, migration 17, atomic capped FIFO admission, exact batch
settlement/rescheduling, binding-safe disconnect purge, and closed-and-drained missing-vault
recovery. It is intentionally not exposed as a partial feature: playback evidence wiring, delivery
and lifecycle ownership, consent/per-source policy, account and status UI, localization, and
release-time production credentials remain. The countable P2.1 record stays open until those layers
and their full acceptance matrix land.

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

The initial implementation includes one active Last.fm account, browser authorization, secure
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

Vault creation, lookup, update, or deletion failure disables request and queue admission
immediately and presents a fixed localized **secure storage unavailable** state. A transient vault
read failure does not silently purge a queue or create another account; delivery remains paused
until the exact record can be recovered. A missing record while preferences claim the feature is
enabled is the same fail-closed state. Corrupt or oversized fields are rejected without including
their contents in diagnostics.

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
Offline state, a timeout, network failure, service code 11/16, malformed response, shutdown, or
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
submitting one queue-insert command. The database transaction rechecks the active account binding
and global queue cap, then commits before any scrobble network request may include that row. A
failed insert or full queue cannot be reconstructed from later events because the occurrence latch
stays closed; it produces a visible fixed-category failure rather than risking duplicate admission.

The queue is capped at exactly 10,000 rows globally for the one active account. At the cap,
Tributary refuses every new qualified row all-or-none and never silently evicts an older scrobble.
Playback continues normally, and the settings status plus a rate-limited toast report that new
scrobbles are not being saved until pending rows are delivered or purged.

## Durable queue and privacy boundary

The SQLite migration adds a strictly recognized, account-bound FIFO. Each row persists only:

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
The migration is transactional and idempotent, validates an already-present table and indexes, and
refuses a downgrade while any queue row exists. An empty queue may be downgraded without leaving
account or listening metadata behind.

The queue itself is private listening history. It uses the existing application data-file
permissions, is included in the consent disclosure, and is purged on explicit Last.fm disconnect.
It is not displayed as a browsable track-history feature or exported through playlist formats.

## Submission, retry, and response handling

One worker owns queue delivery. It acquires the current vault record, verifies the queue binding,
and sends the oldest eligible rows to `track.scrobble` as an HTTPS form POST. One request contains
at most 50 rows, preserving FIFO order and Last.fm's indexed parameter/signature rules. At most one
batch is in flight, so a retrying head cannot be bypassed by new work.

Every request has independent connection and operation deadlines plus a bounded response body.
HTTP status alone is never treated as success; the worker parses the complete Last.fm envelope and
maps each returned scrobble to its request position before changing durable state.

The result policy is closed and exhaustive:

- an explicitly accepted item is terminal-success and is deleted transactionally;
- an item carrying any nonzero `ignoredMessage` code, including an unknown future ignored code, is
  terminal-ignored and is deleted without automatic modification or resubmission;
- top-level service codes 11 and 16 are transient and retain the complete batch for retry;
- DNS, connect, TLS, timeout, response-body interruption, and other failures that leave no complete
  trustworthy response retain the complete batch for retry;
- top-level code 9 retains the complete queue, closes network admission, and pauses delivery until
  the same account is successfully reauthorized;
- every other recognized top-level Last.fm error is terminal for that submitted batch and is not
  retried; and
- a fully received but structurally incoherent success response cannot safely prove which rows
  were accepted. It retains and quarantines the batch, pauses automatic delivery with a visible
  compatibility failure, and never guesses from aggregate accepted/ignored counts.

Accepted and ignored items may be removed independently from one otherwise valid batch response.
The durable mutation is committed before another batch starts. Terminal errors and ignored items
update only bounded aggregate status; raw response text and metadata are not copied into logs or a
failure ledger.

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
   account-scoped retry state; and
4. deletes the vault account record before reporting completion.

If the vault deletion fails, the queue remains purged and all Last.fm work remains disabled; the UI
reports an incomplete secure-store cleanup and offers a retry. Replacing an account uses this same
path. Tributary cannot retract a request Last.fm accepted before cancellation, so the confirmation
explains that already-submitted history remains on Last.fm.

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
- accepted, independently ignored, 11, 16, 9, every other known error, unknown ignored code,
  non-200 success/failure envelopes, malformed complete responses, interrupted responses, timeout,
  offline/restart backoff, ambiguous accepted-before-delete replay, and correction neutrality;
- disconnect races, in-flight response retirement, purge-before-vault-delete failure, normal
  shutdown's database barrier, forced cancellation retention, migration validation/downgrade, and
  secure queue-model redaction; and
- accessible settings states and exact key/placeholder parity across all 13 shipped locale
  catalogs, with no substantive English fallback.

No CI test calls the public Last.fm service. Protocol tests use a bounded local fake HTTPS/service
boundary or a transport abstraction, while the release checklist separately verifies that a
production API account is registered, injected, and permitted under Last.fm's current terms.
