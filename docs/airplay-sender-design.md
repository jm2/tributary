# AirPlay sender design investigation

Status: design record, no implementation in this bead.

Revision 10 (2026-09-13, corrective pass). This revision answers the
round-11 Codex findings at the `cc83477` head with three ordering fixes
to §4.1/§4.3/§9, all docs-only: (1) ticket custody is now atomic with
the `RecoveryPending` transition — the protected lease is moved off the
proxy's active lease into the recovery-custody slot under the proxy's
state lock *as part of producing* the outcome, not by the load path
after it receives it, so a replacement load's
`prepare_with_server_start` can no longer take and revoke the
still-active lease in the interval before the hand-off; (2) explicit
teardown no longer drains the custody slot early — Stop and output
destruction must drive recovery to its terminal quiesced outcome
(terminate/restart the dedicated daemon) or hand custody to the
recovery owner, and the route is revoked only at that terminal,
quiesced outcome; (3) acceptance item 5 is outcome-sensitive, so the
recovery-deadline variant — `RestorationFailed` with no restoration
attempt — is consistent with the revocation ordering it preserves:
`Restored` follows completed restoration, while `RestorationFailed`
follows only the documented settle-or-restart quiescence that already
guarantees no request referencing the route survives. It changes the
design record only.

Revision 9 (2026-09-13, corrective pass). This revision answers the
round-10 Codex findings at the `fa4d660` head with two ordering fixes
to §4.1/§4.3/§9: (1) the recovery-pending branch now hands its media
ticket off from the proxy's active lease into a dedicated
recovery-custody slot before it awaits restoration, because the
pre-existing supersession path (`prepare_with_server_start`,
`src/audio/gstreamer_media.rs:255-271`) takes and unconditionally
revokes the active lease — so a replacement load arriving mid-recovery
would otherwise revoke the still-pending route before restoration ran;
acceptance item 12 exercises exactly that race; (2) `RecoveryCompletion`
now resolves with a terminal outcome — `Restored`, or
`RestorationFailed` when restoration failed and the record was retained
for the supervisor — bounded by a documented recovery deadline (a named
constant in the implementation record), so `wait()` always terminates,
the protected route is never stranded on a persistent restoration
failure, and the caller can safely revoke; acceptance item 13 covers
persistent restoration failure. It changes the design record only.

Revision 8 (2026-09-13, corrective pass). This revision answers the
round-9 Codex findings at the `af12b255` head with two fixes to
§3/§4.1/§9: (1) the recovery-pending failure state now carries a
completion handle, and the ticket ordering is stated per outcome class
— because `Failed(SenderError::RecoveryPending)` returns before
restoration runs, the load path may not revoke its own media ticket on
receipt; it awaits the handle and only then calls `revoke_if_current`,
so revocation still follows transport restoration, and acceptance item
5 asserts both branches; (2) the retained-identifier prerequisite now
also replaces the UI output selector's display-name deduplication and
carries the identifier on each output row through activation, so a
second same-named receiver is selectable and the §9 item 11 mapping is
exercised through the real UI flow. It changes the design record only.

Revision 7 (2026-09-13, corrective pass). This revision answers the
round-8 Codex findings at the `162b263` head with three correctness
fixes to §3/§4.1/§4.3: (1) the cleanup-deadline branch no longer
claims complete teardown — a transmitted mutating RPC that is still
unsettled at the deadline now surfaces as an explicit
`SenderError::RecoveryPending` failure state that defers every
restoration/teardown claim until the request settles or the dedicated
daemon is terminated/restarted, because an implementation cannot both
return within the deadline and prove teardown; (2) crash recovery now
quiesces the dead holder's already-transmitted mutating requests
before restoring or admitting another session — most reliably by
terminating/restarting the dedicated instance, since OS lock release
alone cannot retract a request the daemon is already applying — and
acceptance item 9 asserts it; (3) the daemon path now requires a
retained, correlatable discovery device identifier (the device MAC
OwnTone parses into its `/api/outputs` `id`) and refuses rather than
display-name-matching, so two same-named receivers cannot be
confused. It changes the design record only.

Revision 6 (2026-09-13, corrective pass). This revision answers the
round-7 Codex findings at the `4814ef4` head with two correctness fixes
to §4.1/§4.3: (1) the quiescence-timeout branch no longer claims the
daemon is mutation-free — an unacknowledged `queue/add` or `player/play`
can still execute after the deadline, so recovery stays serialized until
that request settles (or the dedicated daemon is terminated/restarted to
cancel it) before ownership is released, and neither the supervisor nor
a next opener can interleave a restoration with the unsettled request;
(2) the natural-EOS path closes only the pipe write end and defers
stopping/removing the pipe item until daemon-confirmed completion, so
the item can drain naturally instead of being stopped before the poll.
It changes the design record only.

Revision 5 (2026-09-12, corrective pass). This revision answers the
refinery re-review of the `3846818` head with three fixes, two of them
server-side lifecycle corrections to §4.1/§4.3 and one a mechanical
counter correction: (1) cancelling a transmitted *mutating* daemon RPC
does not retract it — the daemon applies `outputs/set`, `queue/add`,
and `player/play` even when the response is never read — so before
`Cancelled` is reported and the lock is released, the adapter now
quiesces those requests within the cleanup deadline (or re-runs the
reversal after the last one settles), and a quiescence miss is a
deadline failure, never `Cancelled`; (2) the natural-EOS path now
waits for daemon-confirmed item completion, bounded by a drain
deadline, before disposing resources, restoring the daemon, or
publishing `TrackEnded`, and a drain timeout or transport loss ends as
error + `Stopped`; (3) the `docs/task.md` and §11 backlog counters are
corrected to 17/57 (29.8%), baseline 17/39. It changes the design
record only.

Revision 4 (2026-09-12, corrective pass). This revision answers the
operator corrective review of the `b1e2b74` head with three lifecycle
fixes to the §4.1/§4.3 contract, each grounded in the existing
media-proxy ownership and the UI event contract: (1) `OpenCancel` now
requires interruptible I/O or a bounded race per blocking operation,
not polls between steps, so a cancellation aborts the operation in
flight and no stale attempt can mutate the daemon or publish after a
replacement load; (2) every non-opened outcome — cancellation
included — revokes its own current media ticket through
`revoke_if_current` after transport restoration, so a cancelled load
leaves no retained loopback route while never touching a newer
replacement's ticket; (3) §4.3 defines natural decode-pump EOS as a
third pump exit distinct from terminal loss, specifying drain,
resource disposal, daemon restoration, and exactly one
generation-scoped `TrackEnded`, with duplicate-EOS and
superseded-generation cases covered by acceptance item §9.10. It
changes the design record only.

Revision 3 (2026-09-09, corrective pass). This revision answers the
operator corrective review of revision 2 at `2cae166` with three
design-gap fixes, each grounded in primary sources rechecked live on
2026-09-09: (1) §4.3 now requires a dedicated, Tributary-owned OwnTone
instance — or, for any documented exception, enforceable exclusive
ownership with continuous output/queue revalidation and safe
restoration — because the JSON API has no sender-session isolation:
one player, one current queue, and a server-wide enabled-output set
([`docs/json-api.md`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/docs/json-api.md)
at release 29.3, added to the pinned sources below); (2) §4.1 adds a
nonblocking position/duration observation seam and cache on
`SenderSession` — with generation, paused, and disconnected semantics
— instead of UI-timer events as the only position evidence (§4.4/§7);
(3) §8 replaces platform scoping inherited from OwnTone's channel
list with an explicit availability decision for every actual
Tributary package target (Fedora RPM/COPR, `.rpm`/`.deb` releases,
Arch AUR, Flatpak, macOS `.dmg`, Windows winget/installer/zip), based
on documented OwnTone acquisition and the dedicated-instance runtime
path, with honest fail-closed behavior elsewhere. The draft/operator
review hold on PR #170 is preserved untouched by this revision; it
changes the design record only.

Revision 2 (2026-09-04, source-backed rewrite). This revision replaces the
2026-07-27 survey after an independent exact-head review rejected it. The
rejected text misstated the classic RAOP model (it confused the `pw` TXT
flag with the RSA public modulus, claimed a TLS-PSK control channel that
classic RAOP does not have, and asserted a per-device mDNS key fetch that
no real RAOP sender performs), omitted the one maintained, packaged,
AirPlay-2-capable sender daemon (OwnTone 29.3) from the survey entirely,
assumed discovery data Tributary does not retain, and shaped the sender
trait around `gst::Element`, which cannot represent a non-GStreamer
process adapter. Every protocol claim below now cites a primary source:
the maintained OwnTone sender implementation itself
([`src/outputs/raop.c`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/outputs/raop.c)
and
[`src/outputs/airplay.c`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/outputs/airplay.c)
at release 29.3, tag commit `d6fb3edf`, 2026-07-22 — including its
JSON API reference, [`docs/json-api.md`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/docs/json-api.md)),
the OwnTone changelog and
installation records, and PipeWire's maintained
[`module-raop-sink`](https://github.com/PipeWire/pipewire/blob/b741e0c74f5436f0c925f7741140db0efd32cf4e/src/modules/module-raop-sink.c).
All upstream source links in this record are pinned to those immutable
release-tag commits (PipeWire 1.6.8, tag commit `b741e0c7`), not to moving
branches; every cited line number was verified against the pinned revision
on 2026-09-08.

Records the P2.4 "Open and complete an AirPlay sender design
investigation" investigation ([`docs/task.md:399-408`](task.md),
entry P2.4-C) and is the first
record on the maintained AirPlay sender path that the P2.4 work stream
must select. The P2.4-C checklist item itself stays unchecked — it is
marked "In flight" against this PR — and closes on a maintainer's
acceptance of the design, not on this document's existence.

The investigation exists because the currently shipped seam
(`uridecodebin ! audioconvert ! avenc_alac ! raopsink`) is gated on a
GStreamer `raopsink` element that **no current official GStreamer,
Homebrew, or MSYS2 package ships** — see the 2026-07-20 sender review
recorded in
[`docs/release-component-policy.md:87-96`](release-component-policy.md)
and the module-level comment in
[`src/audio/airplay_output.rs:30-41`](../src/audio/airplay_output.rs).
The P2.9 remediation removed the broken `shairport-sync`-as-sender
fallback ("[tracker item P2.9](task-remediation-2026-07.md)") and now
reports AirPlay 1 unavailable rather than silently spawning a subprocess
that could never reach the selected receiver.

This document is the *result of the investigation*: a protocol model
grounded in maintained sender source, a survey of the candidates that
could fill the seam, provenance and licensing treatment for each,
packaging consequences, the test contract the seam must satisfy, and a
concrete next-record plan. It deliberately does not implement any one
candidate. Choosing and shipping a sender is a separate bead — the
planning follows once a maintainer accepts one of the proposals below.

## 1. Problem statement

The current seam:

```text
uridecodebin ! audioconvert ! avenc_alac ! raopsink
                                              ^^^^^^^^
                       enforced by AirPlayOutput::ensure_raopsink
                       (src/audio/airplay_output.rs:279-285),
                       availability from raopsink_available
                       (src/audio/airplay_output.rs:266-270),
                       wired through open_session / open_resolved_session /
                       open_local_session / open_prepared_media
                       (src/audio/airplay_output.rs:159-208),
                       built by build_raop_pipeline
                       (src/audio/airplay_output.rs:434-462).
```

Every AirPlay 1 load gates on
`gst::Registry::get().find_feature("raopsink", ...)`. When the element is
absent (the documented current state on every supported PACKAGED
deployment, absent a separately supplied compatible raopsink),
`ensure_raopsink` returns the localized
`errors.playback.airplay_raopsink_missing` error and the pipeline is
never built. Tests `a_missing_raopsink_is_refused_with_honest_guidance`,
`raopsink_guidance_is_localized_for_every_catalog`, and
`a_missing_raopsink_load_fails_loudly_not_silently`
([`src/audio/airplay_output.rs:721-780`](../src/audio/airplay_output.rs))
pin that contract.

The seam itself is fine — it is the *element* that does not exist. To
unblock real AirPlay sending we need either (a) a maintained, packaged
GStreamer `raopsink` element, or (b) a different transmission path that
replaces the `raopsink !` tail with another mechanism. Unlike revision 1
of this record, this revision treats AirPlay 2 as an addressable target:
the recommended path in §6 reaches AirPlay 2 receivers through a
maintained daemon rather than by implementing the protocol in-tree.

## 2. Protocol model (primary-source)

This section is the factual base every candidate below is judged
against. Sources are cited inline; where a claim belongs to the
maintained OwnTone implementation, line references are to
`owntone-server` master at release 29.3.

### 2.1 Discovery: the TXT record is metadata, not key material

RAOP/AirPlay receivers advertise `_raop._tcp.local.` (and AirPlay-2-era
devices also or instead advertise `_airplay._tcp.local.`). The TXT
record carries capability and status flags. OwnTone's sender captures
real examples in a comment block
([raop.c:4174-4198](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/outputs/raop.c)),
e.g. `["sf=0x4" "am=AppleTV2,1" "vs=105.5" "md=0,1,2" "tp=TCP,UDP"
"vn=65537" "pw=false" "ss=16" "sr=44100" "da=true" "sv=false" "et=0,3"
"cn=0,1" "ch=2" "txtvers=1"]`. The fields a sender actually consumes:

- **`pw` — password flag, not a key.** OwnTone parses it as a boolean:
  `rd->has_password = (strcasecmp(p, "false") != 0)` (raop.c:4325-4334).
  The password itself never appears on the network side of discovery;
  the operator configures it in the *sender's* config for the device
  name (raop.c:4336-4347, and the OwnTone AirPlay documentation:
  "For devices that are password-protected, the device's AirPlay name
  and password must be given in the configuration file"). Revision 1's
  claim that `pw` carries "the RSA public modulus, base64" is wrong and
  is retracted.
- **`tp` — transport support.** Receivers that lack `UDP` are discarded
  by OwnTone as non-AirTunes-v2 (raop.c:4297-4315): the modern audio
  path is RTP over UDP.
- **`et` — session-key encryption types**, e.g. `0` (none), `1`
  (RSA/AES), `3`/`4` (FairPlay/MFi-SAP variants, required by some
  third-party devices).
- **`sf` — status flags**, including the device-verification bit OwnTone
  checks (`sf & (1 << 9)` → `requires_auth`, raop.c:4355-4360).
- **`sr`/`ss`/`ch`/`cn`/`md`/`am`** — sample rate, sample size,
  channels, codecs, metadata support, model string.
- **`pk`** — the device's 32-byte Ed25519 public key used by
  AirPlay-2-era pair-verify. It is *not* the classic RAOP audio key
  (§2.2).

### 2.2 Session keys and the provenance of the RAOP RSA key

Classic RAOP (AirPlay 1) audio session establishment is:

1. **RTSP over plaintext TCP** to the receiver's announced port —
   OPTIONS, ANNOUNCE, SETUP, RECORD. There is no TLS and no PSK in this
   channel; the anti-spoofing measure is the Apple-Challenge /
   Apple-Respond header exchange (OwnTone sets `Apple-Challenge` on
   ANNOUNCE, raop.c:1611).
2. **ANNOUNCE carries SDP** with, among others (raop.c:1185-1191):
   - `a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100` — the ALAC framing
     contract; the first parameter is the frames-per-packet count, 352.
   - `a=rsaaeskey:<base64>` — the AES-128 session key, RSA-OAEP
     (SHA-1) encrypted to the receiver's RSA public key (OwnTone builds
     the OAEP-padded encryption with libgcrypt, raop.c:739-864).
   - `a=aesiv:<base64>` — the AES-CBC IV for the audio payload.
3. **RECORD starts the RTP streams**: audio, control, and timing ports
   exchanged in SETUP. Volume later travels as RTSP SET_PARAMETER with
   the receiver's dB convention — OwnTone maps its 0-100 percent scale
   to −30…0 dB with −144 dB as mute (raop.c:2621-2634).
4. **Password-protected devices** authenticate the RTSP session with an
   MD5 digest challenge over the configured password
   (raop.c:899-936). **Device verification** (Apple TV 4 / tvOS 10.2
   and later) is a separate, PIN-mediated flow; OwnTone surfaces it
   through its web interface and implements it with libsodium
   (changelog 25.0).

The critical provenance fact: **the receiver's RSA public key is not
published in mDNS and not fetched per device.** Every practical
classic-RAOP receiver decrypts `a=rsaaeskey` with the *well-known*
AirPort Express RSA key pair, and every practical sender embeds the
matching public half as a constant. OwnTone's sender carries the
2048-bit modulus and exponent verbatim
([raop.c:276-294](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/outputs/raop.c));
receiver projects (shairport-sync and descendants) embed the private
half. Revision 1's assertion that "fetch the public key from the
receiver's mDNS record at runtime" is "the only correct RAOP-1 sender
behavior" is wrong and is retracted — no maintained sender behaves that
way, and the TXT record has no field that carries this key.

Consequence for Tributary: a Tributary-owned classic-RAOP sender must
embed the well-known public modulus as a constant in our source tree.
That is precisely the "sender implementation that embeds protocol key
material" case in the release-component review boundary
([`docs/release-component-policy.md:82-84`](release-component-policy.md)):
"a key being public rather than private does not establish that its
provenance or distribution is appropriate." Embedding it is *possible*
with a dedicated review record, but it is a real cost, and §6 shows the
recommended path avoids it entirely.

### 2.3 Classic RAOP versus AirPlay 2 authentication

- **Classic RAOP (AirPlay 1 audio).** No pairing. Trust is network
  locality plus, optionally, an RTSP password digest and the
  PIN-mediated device-verification flow (§2.2). The only cryptography
  on the audio path is the RSA-OAEP-wrapped AES session key and
  AES-CBC payload encryption — against passive listeners, not against
  unauthenticated senders.
- **AirPlay 2.** Pairing-first. OwnTone's AirPlay 2 sender
  ([`src/outputs/airplay.c`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/outputs/airplay.c))
  runs the pair-setup / pair-verify sequence through its `pair_ap`
  library (`pair_setup_request1/2/3`, airplay.c:2824-2910): an SRP6a
  enrollment followed by Ed25519 identity exchange and X25519-based
  verification, deriving session secrets. Control traffic and audio
  packets are then ChaCha20-Poly1305 encrypted
  (airplay.c:635-662, 1460, 1938), and timing moves from the classic
  UDP timing port to PTP (airplay.c includes `ptpd.h`; changelog 29.1:
  "Samsung and Sonos Era speakers via support for Airplay 2 PTP
  timing", "shairport-sync Airplay 2 mode via support for PTP
  timing"). AirPlay 2 password authentication exists as well
  (changelog 28.5/28.6), and compressed ALAC is supported end to end
  (changelog 27.3, 28.9).

Revision 1 collapsed these two regimes into one incoherent model (it
mixed an RSA-modulus reading of `pw` with a TLS-PSK control channel).
The regimes are distinct: RAOP 1 is announce-with-encrypted-key over
plaintext RTSP; AirPlay 2 is pair-then-encrypt with modern AEAD and
PTP. A sender record that wants AirPlay 2 receivers without writing the
pairing stack itself needs a component that already implements it —
which is the deciding advantage of the OwnTone candidate in §5.4.

### 2.4 Audio packetization: 352-sample frames

The classic RAOP audio frame is **352 samples** per channel at 44.1 kHz
stereo, ALAC-encoded, one frame per UDP packet:

- OwnTone: `#define RAOP_SAMPLES_PER_PACKET 352`, with the comment that
  44100/352 divides evenly (raop.c:82-84), and the ANNOUNCE
  `a=fmtp:96 352 ...` first parameter announces that count.
- OwnTone's FIFO output uses exactly the PCM equivalent:
  `FIFO_PACKET_SIZE 1408 // 352 samples/packet * 16 bit/sample * 2
  channels` at `{ 44100, 16, 2 }`
  ([fifo.c:41,64](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/outputs/fifo.c)).
- PipeWire's `module-raop-sink` independently pins
  `FRAMES_PER_UDP_PACKET 352`
  ([module-raop-sink.c:135](https://github.com/PipeWire/pipewire/blob/b741e0c74f5436f0c925f7741140db0efd32cf4e/src/modules/module-raop-sink.c)).

Older implementations padded each ALAC frame into a fixed 4096-byte
payload; maintained senders ship compressed ALAC instead (OwnTone
changelog 28.9: "use compressed ALAC for Airplay for bandwidth"), so
the invariant a new sender must honor is the **352-sample framing
announced in the fmtp line**, not a byte-padded payload size.

**Implementation risk to carry forward (explicit).** Tributary's
current encoder is `avenc_alac`
([`src/audio/airplay_output.rs:441`](../src/audio/airplay_output.rs)),
whose default ALAC frame size is not 352. Any candidate that keeps
ALAC encoding inside Tributary (the GStreamer `raopsink` adapter is
exempt only because that element performs its own framing) must
constrain the encoder to 352-sample frames or re-frame between encoder
and sender, and must verify the result against a real receiver. This
constraint is recorded in §9 as an acceptance item.

## 3. Discovery alignment: what Tributary actually has

The seam design must be driven by the discovery data that exists in the
tree, not by data revision 1 imagined. Today:

- Discovery browses both services: `RAOP_SERVICE =
  "_raop._tcp.local."` and `AIRPLAY2_SERVICE = "_airplay._tcp.local."`
  ([`src/discovery.rs:261-264`](../src/discovery.rs), browse calls at
  [344-352](../src/discovery.rs)).
- Resolved services become
  [`DiscoveredServer`](../src/discovery.rs) rows with exactly five
  fields (`src/discovery.rs:33-49`): `name`, `url`, `service_type`,
  `requires_password`, `advertised_route`. **No TXT record, no `pw`,
  no `et`/`sf`/`pk`, and no device MAC are retained.** For mDNS
  services the URL is built as `http(s)://{host}:{port}`
  (`src/discovery.rs:524-546`); the AirPlay instance name is the raw
  instance name with the `MAC@` prefix stripped
  (`src/discovery.rs:522-526`,
  [`strip_airplay_mac_prefix`](../src/discovery.rs) at :944).
- The UI path keeps only display name plus endpoint: the airplay row
  stores `"{host}:{port}"` as its widget name
  ([`src/ui/discovery_handler.rs:281-304`](../src/ui/discovery_handler.rs)),
  and output activation reconstructs
  `OutputTarget::AirPlay { host, port }`
  ([`src/ui/output_switch.rs:329-339`](../src/ui/output_switch.rs)).
  Before a row is even built, `handle_airplay_found` dedups by
  **display name** —
  `is_device_in_output_list(output_list, &airplay_name)`
  ([`src/ui/discovery_handler.rs:286`](../src/ui/discovery_handler.rs),
  helper at :439-459, comparing the row label text) — so a second
  receiver advertising the same name is dropped at the selector and
  can never be selected.
- AirPlay-2-tagged rows are dropped at the UI boundary until a sender
  exists ([`src/ui/discovery_handler.rs:45-55`](../src/ui/discovery_handler.rs)).
- `requires_password` is `None` for every discovered AirPlay device;
  nothing populates it from `pw`.

Consequences for the design:

1. **The sender contract takes `{ display_name, host, port }` — plus,
   for the daemon path, the retained device identifier of consequence
   4 — and nothing else.** Any protocol decision that needs `pw`,
   `et`, `sf`,
   or `pk` must either be delegated to a component that re-resolves the
   device itself (the daemon-based path in §5.4 keeps its own device
   table), or proceed without the flag and surface the receiver's
   refusal (e.g. an RTSP 401 from a password device) as a
   user-actionable error (§9).
2. **Extending discovery is a named, separate change, not an
   assumption.** If a later record wants `pw`/`sf` retained on
   `DiscoveredServer`, that record must extend `process_mdns_event`
   (`src/discovery.rs:488`), widen the `DiscoveredServer` schema, and
   add tests — it may not silently depend on TXT fields that are
   currently dropped.
3. **AirPlay 2 rows stay dropped until the selected path can actually
   play to them**; flipping that filter is part of the implementation
   record for whichever path ships AirPlay 2 (§6), together with a
   discovery change if receiver dedup needs the device id.
4. **The daemon path requires a retained, correlatable device
   identifier before it can be enabled.** OwnTone derives its
   `/api/outputs` `id` from the receiver's device MAC: for the
   `_raop._tcp` output it parses the leading hex `MAC@` prefix of the
   mDNS instance name (`src/outputs/raop.c:4220` `safe_hextou64`,
   `:4272` `rd->id = id`), and for the `_airplay._tcp` output it
   parses the TXT `deviceid` MAC (`src/outputs/airplay.c:3945-3957`,
   `:4004`). Its JSON `output` object exposes that `id` and a `name`
   but no receiver host/port (pinned `docs/json-api.md`, §Get a list
   of available outputs). Display names are not unique — two
   receivers can share one — so name-matching cannot disambiguate and
   can stream to the wrong device. Enabling the daemon path therefore
   carries a prerequisite discovery change (consequence 2, §10 item
   3): retain the normalized device MAC/`deviceid` on
   `DiscoveredServer`, carry it through to the seam's target type
   (consequence 1), **replace the selector's display-name
   deduplication with that identifier and retain it on each output
   row through activation** — otherwise the second same-named
   receiver is discarded by `is_device_in_output_list`
   (`src/ui/discovery_handler.rs:286`, :439-459) before any
   identifier could be retained, and the mapping below can never be
   exercised through the real UI — and map the selected receiver to
   OwnTone's output by that identifier, refusing when it is absent or
   does not resolve to exactly one output (§4.3 receiver selection,
   §9 item 11).

## 4. Seam design

The seam keeps its structural shape — a fail-closed availability gate
before any per-track proxy work, a per-session transport tied to the
generation-scoped event channel, and bus-forwarded state — but the
sender abstraction must stop leaking GStreamer types. A process
adapter (a spawned or system daemon fed over a pipe) cannot be
expressed as a `gst::Element`, and revision 1's trait
(`build_sink_tail(...) -> Result<gst::Element, String>`) made that
whole class of candidate unrepresentable.

### 4.1 Proposed sender contract

```rust
/// One live AirPlay session, already negotiated with the receiver.
/// Implementations own their transport; Tributary only pushes audio
/// and control. `Send` because sessions outlive the UI thread.
trait SenderSession: Send {
    /// Push interleaved s16le 44100 Hz stereo PCM into the session.
    /// The outcome separates retryable backpressure from terminal
    /// session loss — a byte count cannot express both, and the
    /// daemon adapter's FIFO produces hard write errors (a closed
    /// reader fails with `EPIPE`) that must never be retried:
    ///
    /// - `Accepted(n)` — `n` bytes consumed; the caller continues
    ///   with the remainder.
    /// - `Backpressure` — the session is healthy but temporarily
    ///   unable to accept audio (full adapter buffer, or an open
    ///   FIFO whose reader is momentarily slow). The decode pump
    ///   wakes and retries; this is today's `0` shape, unchanged.
    /// - `Terminal(reason)` — the session can never accept audio
    ///   again (FIFO reader closed, daemon gone, receiver session
    ///   failed). Before returning, the adapter publishes the
    ///   generation-tagged `PlayerEvent::Error` + `Stopped` pair
    ///   (§4.1 events; §9.4 session-loss contract) and wakes the
    ///   decode pump exactly once; the pump stops feeding this
    ///   session and drops it through `close`. A terminal outcome
    ///   is never retried and never reported as `0`.
    ///
    /// A session that sources its own decoder from the prepared URI
    /// (§4.2) owns its decode internally, never consumes pushed
    /// audio, and documents `write_pcm` as an unsupported no-op for
    /// its type.
    fn write_pcm(&mut self, samples: &[u8]) -> SenderWriteOutcome;
    /// Receiver-facing volume in [0.0, 1.0]; the adapter owns the
    /// mapping to its protocol's convention (§2.2: RAOP dB, mute at
    /// 0.0).
    fn set_volume(&mut self, level: f64);
    fn pause(&mut self);
    fn resume(&mut self);
    /// Flush buffered audio without tearing down the receiver session.
    fn flush(&mut self);
    /// Nonblocking position/duration observation. Implementations
    /// maintain the snapshot on their own task — the GStreamer
    /// adapter samples its pipeline there, the daemon adapter samples
    /// the JSON API player progress — and this method only reads the
    /// latest cached snapshot. It performs no I/O and must never
    /// block the caller: the 500 ms UI timer (§4.4) publishes from
    /// this cache; the timer is not the position source.
    fn observe(&self) -> SenderPosition;
    /// Tear down the receiver session and local resources. Consumes
    /// self so a closed session is unrepresentable.
    fn close(self: Box<Self>);
}

/// Outcome of one `write_pcm` call (§4.1). The decode pump (§4.3)
/// treats `Backpressure` as retryable and `Terminal` as final; the
/// distinction is the pump's only guarantee that it will not spin
/// forever on a session that has already died. Natural
/// end-of-stream is deliberately *not* a variant: the pump detects
/// EOS from its own decode pipeline and completes the track through
/// the §4.3 natural-completion path, which is distinct from both
/// retryable backpressure and terminal session loss.
enum SenderWriteOutcome {
    /// `n` bytes accepted; continue with the remainder.
    Accepted(usize),
    /// Healthy but momentarily full; wake and retry.
    Backpressure,
    /// The session failed terminally. The adapter has already
    /// published the generation-tagged error + `Stopped` events and
    /// woken the pump exactly once before this value is returned.
    Terminal(String),
}

/// The latest position/duration snapshot a session has published.
/// Values are meaningful only for the generation the session was
/// opened for; the publisher (§4.4) drops snapshots from any other
/// generation.
struct SenderPosition {
    generation: PlayerEventGeneration,
    /// `None` while the adapter has no trustworthy value: before the
    /// first confirmed sample, or once the snapshot went stale.
    position_ms: Option<u64>,
    duration_ms: Option<u64>,
    /// Set once the underlying source can no longer be confirmed
    /// (receiver lost, daemon unreachable). A stale snapshot freezes
    /// at its last confirmed values and must never be extrapolated.
    stale: bool,
}

/// Terminal outcome of the serialized recovery behind a
/// `Failed(SenderError::RecoveryPending)` open. `wait()` is bounded,
/// so the handle always ends in one of these states, never in an
/// indefinite wait and never in a state that reports a completed
/// unwind it did not perform.
enum RecoveryOutcome {
    /// Restoration ran and the incomplete-takeover record cleared;
    /// ownership was released. The caller may revoke its ticket.
    Restored,
    /// Restoration did not complete: a restoration step failed, or
    /// the recovery window reached the recovery deadline first, and
    /// the incomplete-takeover record was left in place for the
    /// supervisor to retry (§4.3). Recovery quiesces the dedicated
    /// daemon before it runs restoration, and its bounded fallback —
    /// terminating and restarting the instance, which drops every
    /// connection and cancels any in-flight request — is what makes
    /// that quiescence reachable even when a mutating request never
    /// settles on its own. So no request that referenced the loopback
    /// route survives recovery, and the retained record's recovery
    /// does not depend on the route: the caller may stop waiting and
    /// revoke its own ticket. Terminal, and explicitly not a success.
    RestorationFailed { message: String },
}

/// Completion handle for the serialized recovery that a
/// `Failed(SenderError::RecoveryPending)` open leaves behind. That
/// outcome returns while restoration is still pending, so it cannot
/// report a completed unwind; carrying this handle is how the load
/// path learns how the unwind actually ended. It resolves with a
/// terminal `RecoveryOutcome` once the adapter's recovery has run
/// restoration — the outstanding mutating request settled, or the
/// dedicated daemon was terminated/restarted to cancel it — and
/// either released the incomplete-takeover record (`Restored`) or
/// retained it for the supervisor after a restoration failure
/// (`RestorationFailed`), whichever happened within the documented
/// recovery deadline (§4.1, §4.3). That deadline is a named constant
/// in the implementation record, distinct from the cleanup deadline:
/// it caps the whole serialized recovery after `RecoveryPending` —
/// the settle-or-restart quiescence plus the restoration attempt —
/// so an unbounded restoration retry cannot keep the handle pending.
/// It is cloneable and safe to hold
/// across threads, so the load path may await it inline or register a
/// continuation; either way the wait terminates, because recovery is
/// bounded by settle-or-restart **and** by that deadline.
struct RecoveryCompletion { /* the implementation record's primitive */ }

impl RecoveryCompletion {
    /// Block until the serialized recovery reaches a terminal state:
    /// restoration ran and the record cleared (`Restored`), or
    /// restoration failed — or the recovery deadline passed — and the
    /// record was retained for the supervisor (`RestorationFailed`).
    /// Returns immediately if recovery already finished. Safe from any
    /// thread, and never reports `Restored` before restoration has
    /// actually run. Callers that must not block register a
    /// continuation instead (the concrete primitive is the
    /// implementation record's choice).
    fn wait(&self) -> RecoveryOutcome;
}

/// Stable, machine-distinguishable seam failures. The load path's
/// deadline, dependency, authentication, and receiver-failure
/// contracts (§9.1, §9.6) are error *kinds* callers must branch on,
/// and a bare `String` cannot carry a kind. Every variant still
/// carries the user-actionable, localized message the load path
/// surfaces verbatim (today's
/// `errors.playback.airplay_raopsink_missing` contract,
/// generalized): the kind routes behavior, the message informs the
/// user.
enum SenderError {
    /// A documented probe/open deadline (a named constant in the
    /// implementation record) was exceeded, and the server side
    /// quiesced inside the cleanup deadline, so everything the
    /// attempt created was torn down through the restoration path
    /// before this variant is returned (§4.1).
    Deadline(String),
    /// The cleanup deadline was missed while a transmitted mutating
    /// daemon RPC was still unsettled. This is deliberately not a
    /// clean unwind and not a plain deadline: no claim is made that
    /// everything the attempt created was torn down, because the late
    /// request can still land. The incomplete-takeover record stays
    /// in place and recovery stays serialized until the request
    /// settles — or the dedicated daemon is terminated/restarted to
    /// cancel it — before restoration runs and ownership is released
    /// (§4.1, §4.3). Callers surface this as recovery-pending
    /// guidance, distinct from `Deadline`. The variant also carries
    /// the `RecoveryCompletion` handle for the serialized recovery,
    /// because the load path owes two orderings. First, its own media
    /// ticket may be revoked only after recovery reaches a terminal,
    /// quiesced outcome, and on this branch that has not happened yet.
    /// The ticket is therefore moved off the proxy's active lease into
    /// recovery custody under the proxy's state lock **as part of the
    /// transition that creates this variant** — not by the load path
    /// after it receives the outcome — so a replacement load's
    /// unconditional supersession revocation
    /// (`src/audio/gstreamer_media.rs:255-271`) can never observe it in
    /// the active lease and invalidate the route in the interval before
    /// the hand-off (§4.1). The load path then awaits the handle and
    /// revokes only at its terminal outcome. Second, that await is
    /// bounded: the handle is terminal, resolving `Restored` or
    /// `RestorationFailed`, so the load path always stops waiting and
    /// can safely revoke rather than stranding the route (§4.3).
    RecoveryPending {
        /// The user-actionable, localized recovery-pending guidance
        /// the load path surfaces immediately.
        message: String,
        /// Resolves with the terminal recovery outcome: restoration
        /// ran and the incomplete-takeover record cleared, or
        /// restoration failed and the record was retained for the
        /// supervisor.
        completion: RecoveryCompletion,
    },
    /// The dependency is absent, unreachable, unsupported on this
    /// platform, or not the dedicated Tributary-owned instance the
    /// daemon adapter requires (§4.3). Includes today's
    /// missing-`raopsink` refusal.
    Dependency(String),
    /// The receiver requires pairing, a password, or PIN
    /// verification that has not succeeded (§9.6).
    Authentication(String),
    /// The receiver-side session failed after negotiation began:
    /// the device vanished, refused the stream, or the daemon
    /// reported a session error.
    Receiver(String),
}

/// Cancellation currency for one `open_session` call, owned by the
/// load path. Setting it is synchronous and idempotent; observing it
/// must interrupt *the operation currently in flight*, not merely the
/// gap after it. The in-flight call therefore does not rely on
/// polling between blocking steps: every blocking operation —
/// ownership re-verification, each RTSP handshake read/write, each
/// daemon RPC, FIFO and API setup, even the lock-acquisition wait —
/// is performed either on a transport the cancel path can abort
/// (socket `shutdown`/close, a FIFO write end it can close) or as a
/// bounded wait raced against this flag (a `select`/timeout that
/// returns the moment the flag is set). When `cancel()` fires, the
/// call aborts the operation in flight and unwinds through the §4.3
/// restoration path within the documented cleanup deadline. Aborting
/// a *mutating* daemon RPC that has already been transmitted is not
/// the same as retracting it: the daemon applies the request whether
/// or not the client reads the response, so dropping a late response
/// cannot prevent the mutation. Before `Cancelled` is reported — and
/// before the §4.3 lock is released — the unwinding call therefore
/// either waits, bounded by the cleanup deadline, for acknowledgement
/// that every transmitted mutating RPC has settled, or re-runs a
/// compensating restoration *after* the last in-flight mutating RPC
/// settles, so no mutation can land on state the restoration already
/// unwound. A mutating RPC that has neither settled nor acknowledged
/// within the deadline makes the outcome
/// `Failed(SenderError::RecoveryPending)`, never `Cancelled` and
/// never a plain `Deadline`, and leaves the §4.3 incomplete-takeover
/// record in place. That branch is *not* a claim that no mutation landed: a
/// `queue/add` or `player/play` transmitted but unacknowledged at
/// the deadline can still execute after the method returns, so the
/// adapter keeps recovery serialized until that request settles —
/// or terminates/restarts the dedicated daemon to cancel it —
/// before releasing ownership, and a supervisor or next opener
/// cannot interleave a restoration with the unsettled request. No
/// event is published for the cancelled generation. Each blocking
/// operation still carries its own documented deadline, so an
/// un-cancelled call that stalls ends as
/// `Failed(SenderError::Deadline)` instead of hanging.
/// The load path sets the flag when the load is dropped or its
/// generation is superseded: exactly the conditions the event
/// contract already keys on. This is deliberately not
/// `PlayerEventGeneration` itself — a generation
/// (`src/audio/mod.rs:85-87`) is a `Copy` value the caller compares
/// after the fact, not a flag an in-flight call can observe, and a
/// synchronous signature that carries only the generation cannot
/// express the abort this contract promises (the module's
/// non-blocking rule, `src/audio/mod.rs:5`). The concrete type is
/// the implementation record's choice (an `Arc<AtomicBool>`-shaped
/// flag plus per-operation abort handles, or the runtime's
/// cancellation primitive); the contract requires only that setting
/// it is synchronous, idempotent, and safe from any thread, and that
/// an in-flight operation can be aborted or raced against it.
struct OpenCancel { /* the implementation record's primitive */ }

impl OpenCancel {
    /// Synchronous, idempotent, safe from any thread. After it
    /// returns, any in-flight `open_session` observes the flag
    /// immediately — aborting the blocked operation itself, not the
    /// step after it — and unwinds through the restoration path
    /// within the documented cleanup deadline.
    fn cancel(&self);
    /// Cheap non-blocking pre/post check. It does not by itself
    /// satisfy the interruption contract: each blocking operation
    /// must still be abortable or raced (see the type docs).
    fn is_cancelled(&self) -> bool;
}

/// Outcome of `open_session`. Cancellation is its own outcome, not
/// an error: a cancelled open never surfaces as a user-facing
/// failure — no `SenderError` kind is invented for "the caller
/// changed its mind", and no error event is published for the
/// cancelled generation — while remaining machine-distinguishable
/// so the load path can skip its failure-reporting branch entirely.
enum OpenOutcome {
    /// Negotiation completed; the session is live.
    Opened(Box<dyn SenderSession>),
    /// Cancellation was observed before negotiation completed, and
    /// the operation in flight was aborted rather than awaited.
    /// Every transmitted mutating RPC settled, or was compensated by
    /// a restoration re-run after it settled, before this variant was
    /// returned, so no mutation from the cancelled generation can
    /// land after the teardown (§4.1). Everything the attempt created
    /// — receiver session, queue items, enabled-output changes — was
    /// already torn down through the same restoration path as failure
    /// (§4.1, §4.3) before this variant is returned, and the load
    /// path revoked its own current media ticket (`revoke_if_current`;
    /// §4.1, §9.5) without touching a newer replacement's.
    /// `Cancelled` is a terminal, fully-unwound state, never a
    /// half-open session and never a retained loopback route.
    Cancelled,
    /// A failure inside the seam's taxonomy. `Failed(Deadline)`,
    /// `Failed(Dependency)`, `Failed(Authentication)`, and
    /// `Failed(Receiver)` carry the failure path's teardown
    /// guarantee — restoration has run before the value is returned.
    /// `Failed(RecoveryPending)` is the one exception and says so:
    /// it reports a cleanup deadline missed with a transmitted
    /// mutating RPC still unsettled, so no complete-teardown claim is
    /// made. The incomplete-takeover record stays in place and
    /// recovery stays serialized until that request settles or the
    /// dedicated daemon is terminated/restarted, after which
    /// restoration runs and the record clears (§4.1, §4.3). The
    /// variant carries the `RecoveryCompletion` handle, which resolves
    /// with a terminal outcome — `Restored` once restoration has run
    /// and the record cleared, or `RestorationFailed` when restoration
    /// failed — or the recovery deadline passed — and the record was
    /// retained for the supervisor. The load path surfaces the
    /// localized recovery-pending guidance rather than a
    /// completed-unwind message; its media ticket was already moved
    /// off the proxy's active lease into recovery custody as part of
    /// producing this variant, so no replacement load can revoke it
    /// early, and the load path revokes it only once the handle
    /// resolves — on either outcome, because the daemon was quiesced
    /// before restoration ran, or before the deadline resolved, and
    /// the retained record's recovery does not depend on the route
    /// (§4.1 ordering).
    Failed(SenderError),
}

/// A selectable transmission path. One immutable instance per
/// protocol/backend, chosen at load time by configuration — never
/// silently, never per-track.
trait AirplaySender: Send + Sync {
    fn name(&self) -> &'static str;
    /// `Ok(())` when this sender can transmit on this host. `Err`
    /// is a `SenderError`: the variant tells callers which failure
    /// kind fired, and its payload is the user-actionable guidance
    /// the load path surfaces verbatim (this is today's localized
    /// `airplay_raopsink_missing` contract, generalized).
    ///
    /// Bounded by contract: `probe` performs only the documented
    /// discovery/health checks of §8 ("Probe reflects reality",
    /// including the §4.3 dedicated-instance ownership check, which
    /// runs before any receiver state is read), enforces the
    /// adapter's documented probe deadline (a named constant the
    /// implementation record states; the seam never leaves a load
    /// pending on an unbounded check), and surfaces a deadline
    /// overrun as `SenderError::Deadline` — never by
    /// blocking. It holds no receiver-session resources, so
    /// cancellation (the load being dropped or its generation
    /// superseded) needs no protocol cleanup: dropping the call in
    /// flight is the whole cleanup.
    fn probe(&self) -> Result<(), SenderError>;
    /// Negotiate a session with the receiver at `host:port`, sourcing
    /// audio from the prepared media at `prepared_uri`, and return
    /// it. Called only after `probe` succeeded and after media
    /// preparation, so negotiation failures are receiver-side
    /// failures; ownership re-verification failures return
    /// `SenderError::Dependency`. Ownership of the dedicated instance
    /// (§4.3) is re-verified from the same supervision record before
    /// the lock is taken, so a daemon swapped in after probe fails
    /// closed before any state is read or mutated. The seam must
    /// carry `prepared_uri`
    /// because a sender is one immutable instance per backend — never
    /// per track — so it cannot capture the URI anywhere else; it is
    /// the same loopback URL today's `open_prepared_session` passes
    /// to `build_raop_pipeline`
    /// (`src/audio/airplay_output.rs:231-241`). Ticket revocation for
    /// every non-opened outcome — failure and cancellation alike —
    /// stays in the load path (`open_prepared_media`, :199-208),
    /// which is where today's failure path already calls
    /// `revoke_if_current`. Dropping the call is *not* sufficient:
    /// `GstreamerMediaProxy` retains its own `Arc` in `state.active`
    /// (`src/audio/gstreamer_media.rs:69-72`), independent of the
    /// discarded load, so a cancelled open that skipped revocation
    /// would leave the authenticated loopback route and its
    /// resources live until the next load or output destruction.
    /// `revoke_if_current` is identity-checked against the proxy's
    /// newest active lease (`src/audio/gstreamer_media.rs:319-337`),
    /// so a load can only ever revoke its own current ticket and never
    /// a newer replacement's. **The ordering is transport restoration
    /// first, revocation after**, so the receiver session is unwound
    /// before the loopback route is invalidated — and the two outcome
    /// classes satisfy it differently, which the seam contract now
    /// states explicitly:
    ///
    /// - `Cancelled` and `Failed(Deadline)`, `Failed(Dependency)`,
    ///   `Failed(Authentication)`, and `Failed(Receiver)` report
    ///   restoration complete, so the load path revokes its own ticket
    ///   via `revoke_if_current` as the outcome returns, exactly as
    ///   today.
    /// - `Failed(SenderError::RecoveryPending)` reports the opposite:
    ///   restoration has *not* run. Revoking on receipt would
    ///   invalidate the loopback route while the unsettled request or
    ///   the not-yet-unwound receiver session can still reference it,
    ///   so the load path must **not** revoke on receipt. The ticket is
    ///   instead moved off the proxy's active lease
    ///   (`src/audio/gstreamer_media.rs:69-72`) into recovery custody
    ///   **as part of the transition that produces this variant** — the
    ///   seam carries the app-owned custody handle alongside
    ///   `prepared_uri`, and the recovery transition performs the
    ///   move under the proxy's state lock before the outcome is
    ///   constructed, so no other load can observe the lease as active
    ///   in between — and the load path then awaits the
    ///   `RecoveryCompletion` handle it carries. The transfer is not a
    ///   post-receipt step by the load path: the proxy's replacement
    ///   path takes the active lease and unconditionally calls
    ///   `previous.revoke()` (`prepare_with_server_start`,
    ///   `src/audio/gstreamer_media.rs:255-271`) — not an
    ///   identity-checked revocation — so a ticket still active in the
    ///   interval between the recovery decision and the hand-off would
    ///   be revoked before this recovery ran. The proxy therefore
    ///   exposes the custody slot and a single atomic transition over
    ///   it, and `prepare_with_server_start` acquires the same state
    ///   lock: a replacement either supersedes the generation before
    ///   the recovery transition (in which case the open observes the
    ///   supersession and cannot return `RecoveryPending` for that
    ///   lease) or finds the lease already custodied and removes
    ///   nothing. Custody is a dedicated slot the replacement path
    ///   never touches, and **explicit teardown does not drain it
    ///   early**: Stop (`GstreamerMediaProxy::revoke`) and the proxy's
    ///   `Drop` must not revoke a custodied route while recovery is
    ///   unresolved. They either drive recovery to its terminal
    ///   quiesced outcome first — synchronously requesting the
    ///   adapter's terminate-and-restart quiescence, which cancels the
    ///   unsettled request within the bounded recovery — or hand
    ///   custody to the recovery owner, which revokes at that terminal
    ///   outcome. The load path revokes the custodied route exactly
    ///   once, when the handle resolves — after restoration on
    ///   `Restored`, and on `RestorationFailed` too, where quiescence
    ///   (settle-or-restart) already guarantees no request referencing
    ///   the route survives and the retained-record recovery does not
    ///   depend on the route — through the identity-checked path
    ///   (`revoke_if_current`, `src/audio/gstreamer_media.rs:319-337`);
    ///   because the ticket is no longer the active lease that call
    ///   degenerates to a direct revoke of the load path's own route
    ///   and can never touch a newer replacement's. The route is
    ///   retained exactly as long as the ordering requires, and no
    ///   route is leaked because the handle always terminates: recovery
    ///   is bounded by settle-or-restart **and** by the documented
    ///   recovery deadline, after which it reports `RestorationFailed`
    ///   rather than waiting forever (§4.3).
    ///
    /// Bounded and interruptible by contract: the call enforces the
    /// adapter's documented open deadline (again a named constant
    /// the implementation record states), and cancellation — the
    /// load being dropped or its generation superseded mid-call —
    /// aborts *the operation in flight* rather than waiting for it to
    /// return. Each blocking operation (ownership re-verification,
    /// every RTSP handshake step, every daemon RPC, FIFO/API setup,
    /// the lock-acquisition wait) is abortable (transport
    /// `shutdown`/close, FIFO close) or raced against `cancel` with a
    /// bounded wait. Aborting a transmitted mutating daemon RPC does
    /// not retract it — the daemon applies the request even when the
    /// response is never read — so, before the outcome surfaces, the
    /// call quiesces the server side: it waits, bounded by the cleanup
    /// deadline, for acknowledgement that every transmitted mutating
    /// RPC has settled, or re-runs the restoration *after* the last
    /// in-flight mutating RPC settles, so no mutation from the stale
    /// generation can land on state the restoration has unwound or
    /// after a replacement load has taken over. A mutating RPC that
    /// neither settles nor acknowledges within the deadline makes the
    /// outcome `Failed(SenderError::RecoveryPending)` — never
    /// `Cancelled`, never a plain `Deadline` — because there is no
    /// clean unwind to report: the unacknowledged `queue/add` or
    /// `player/play` can still execute, so no claim is made that
    /// everything the attempt created was torn down. That branch
    /// leaves the §4.3 incomplete-takeover record in place and keeps
    /// recovery serialized until the request settles — or the
    /// dedicated daemon is terminated/restarted to cancel it — before
    /// any ownership is released and restoration runs, so neither the
    /// supervisor nor a next opener can interleave a restoration with
    /// the unsettled request. The stale attempt publishes no event for
    /// its generation. Every *other* non-opened outcome restores
    /// within the documented cleanup deadline, before the outcome
    /// surfaces: the implementation tears down everything it created
    /// so far (receiver session, queue items, enabled-output changes)
    /// through the same restoration path (§4.3); a caller-requested
    /// abort whose server side quiesced inside the deadline is
    /// `Cancelled`; and an open/probe deadline miss with no unsettled
    /// mutating RPC is `Failed(SenderError::Deadline)`, both with
    /// restoration complete. On the daemon adapter a cancellation
    /// landing mid-takeover records and reverses its steps through the
    /// same incomplete-takeover discipline as a crash (§4.3). The
    /// method never returns a half-open session, and a load can never
    /// remain pending on it indefinitely: after timeout the outcome is
    /// `Failed(SenderError::Deadline)` carrying explicit, localized
    /// guidance (§9.1 contract), or `Failed(SenderError::RecoveryPending)`
    /// when a mutating RPC is still unsettled, carrying the
    /// recovery-pending guidance and the `RecoveryCompletion` handle
    /// the load path awaits before ticket revocation. `Cancelled` and
    /// `Deadline` stay
    /// distinct outcomes, so the UI never renders a cancelled load as
    /// an error and never reports one (§9.5); a caller-requested abort
    /// whose quiescence wait misses the deadline is `RecoveryPending`,
    /// not `Cancelled`, because the unwind cannot be proven complete.
    /// If cancellation and the deadline fire in the same window, the
    /// call returns whichever it observed first and never both.
    fn open_session(
        &self,
        target: &AirplayTarget,
        prepared_uri: &str,
        event_tx: async_channel::Sender<PlayerEvent>,
        generation: PlayerEventGeneration,
        cancel: &OpenCancel,
    ) -> OpenOutcome;
}
```

Key differences from revision 1, and why:

- **No `gst::Element` anywhere in the contract.** The GStreamer
  adapter (§4.2) internally keeps the existing pipeline, sourced from
  the `prepared_uri` the seam carries; the daemon adapter (§4.3)
  feeds its pipe from a decode pump the adapter owns. Neither exposes
  its transport type.
- **PCM in, not ALAC in — with decode-and-pump ownership assigned.**
  The pushed-audio contract is s16le 44100/2 PCM, the format the
  pump-fed boundary accepts (OwnTone's pipe input: "read a PCM16
  stream from a named pipe";
  [`src/inputs/pipe.c`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/inputs/pipe.c);
  its fifo output quality is `{44100, 16, 2}`). Who decodes is
  explicit, per adapter: the daemon adapter owns the decode pump that
  produces the PCM it pushes (§4.3), while the GStreamer adapter's
  first refactor is pipeline-sourced — it consumes `prepared_uri`
  through the seam, keeps `uridecodebin ! audioconvert !
  avenc_alac ! raopsink` intact (decode included), and is driven by
  its pipeline, not by pushed PCM. Encoding and framing stay inside
  adapters — where the 352-sample contract (§2.4) lives — never in
  the shared seam.
- **`open_session` returns a session, not a sink element — and
  cancellation as an outcome, not an error.** Pause / resume /
  volume / flush are protocol operations (RTSP SET_PARAMETER /
  PAUSE, daemon RPC), not pipeline state writes, so they belong to
  the session object; and the open call carries an interruptible
  `OpenCancel` handle and returns `OpenOutcome`, whose `Cancelled`
  variant keeps "the caller changed its mind" out of the
  `SenderError` taxonomy entirely (§4.1).
- **Events stay generation-scoped.** Adapters receive the event
  channel and the load's generation so receiver-side failures
  (reconnect exhaustion, auth refusal) surface through the exact same
  `PlayerEvent::Error` + `Stopped` shape the tests pin today
  (§9).
- **A position/duration observation seam, not timer-only evidence.**
  `SenderSession::observe` returns the session's cached snapshot
  without I/O. Each adapter owns how the cache is produced — the
  GStreamer adapter samples pipeline state on a session-owned task,
  the daemon adapter samples `item_progress_ms` for position and
  sources duration from its own decode pipeline (§4.3), with the
  JSON `item_length_ms` as fallback/cross-check only — while the
  seam owns the semantics: paused sessions freeze their position
  instead of advancing it, disconnected sessions set `stale` instead
  of extrapolating, and snapshots from a foreign generation are
  dropped by the publisher. The UI's 500 ms timer remains the
  *publisher* (§4.4); it reads the cache rather than being the only
  measurement.

### 4.2 GStreamer adapter (`raopsink`)

Wraps today's path: `probe` is `raopsink_available`
(`src/audio/airplay_output.rs:266-270`) behind `ensure_raopsink`
(:279-285) semantics; the session consumes the `prepared_uri` carried
through the seam and runs the whole existing
`uridecodebin ! audioconvert ! some-alac-enc ! raopsink` pipeline,
reusing today's bus watch (:307-376) unchanged, with the position
timer (:384-412) becoming the producer of the session's §4.1
observation cache at the same 500 ms cadence. **Decode-and-pump
ownership is the pipeline's:**
`uridecodebin` fetches and decodes the prepared URI inside the
session, so this session never consumes `write_pcm` — the trait
documents a pipeline-sourced session's `write_pcm` as an unsupported
no-op, and pause/resume/volume/flush map onto pipeline state and the
RAOP volume exactly as today's session. Zero new dependencies;
quality equal to today's; subject to `raopsink` never being packaged
(policy record,
[`docs/release-component-policy.md:87-96`](release-component-policy.md)).
The `appsrc`-bridged variant whose hot path *is* `write_pcm` stays
deferred until §4.3's pump exists and a record needs it; it is not
part of the first refactor.

### 4.3 Process adapter (OwnTone daemon)

Tributary talks to an OwnTone instance as a transmission service:

- **Transport in:** OwnTone's pipe input — a named pipe holding raw
  PCM16, startable by selecting it or autostarted
  (`src/inputs/pipe.c`: "This module will read a PCM16 stream from a
  named pipe"; `pipe_autostart`). The adapter's `write_pcm` is a FIFO
  write: backpressure is natural and stays retryable
  (`SenderWriteOutcome::Backpressure`, §4.1), while a closed reader
  (`EPIPE`) or a dead daemon is `Terminal` — the pump stops and the
  §9.4 session-loss contract fires, never a retried `0`. **Decode
  and pump ownership is the adapter's:** it owns a headless decode
  pipeline
  (`uridecodebin ! audioconvert ! appsink`, caps
  `audio/x-raw, format=s16le, rate=44100, channels=2`) sourcing the
  same `prepared_uri` the seam carries, and pumps the decoded PCM
  into `write_pcm`. The pump wakes on backpressure and stops on the
  first terminal outcome; it holds no retry loop across a terminal
  session. **The pump has three exits, and they must not be
  conflated:** `Backpressure` (healthy, wake and retry), `Terminal`
  (session loss — §9.4 error + `Stopped`), and **natural EOS**.
  Natural EOS is detected by the pump from its own decode pipeline
  (`appsink`/bus `EOS`), never inferred from `write_pcm` — the
  `SenderWriteOutcome` taxonomy stays backpressure/terminal only.
  On natural EOS of a finite track the adapter: drains the decoded
  remainder through `write_pcm` until it is accepted (the pipe is
  not truncated mid-frame) and closes only the pipe write end so the
  daemon sees end-of-input; it does not stop or remove the pipe item
  before the completion poll, because stopping the daemon player
  early would prevent the item draining naturally and recreate the
  truncated-tail bug. Then, before touching resources or daemon
  state, it waits, bounded by a drain deadline, for the daemon to
  confirm the item actually completed. The
  acceptance of the final `write_pcm` proves only that the FIFO
  write succeeded, not that OwnTone (or the receiver) rendered those
  samples, so disposing the pipe item and restoring the player on a
  client-local EOS alone can truncate the track tail and report
  completion early. The wait polls the daemon's own player/item
  state through its JSON API (`/api/player`) until the item reports
  finished with its end-of-input consumed — never from the client's
  closed write end alone. A drain timeout or transport loss during
  the wait is not natural completion and must not publish
  `TrackEnded`: it ends as failure per §9.4 (`PlayerEvent::Error` +
  `Stopped`), because the tail was not proven rendered. Only after
  daemon-confirmed completion does the adapter dispose of the owned
  pipe/queue/session resources, restore the dedicated daemon to
  the state recorded at takeover (player stopped, our queue items
  removed, the recorded enabled-output set re-applied, lock
  released), and publish **exactly one** generation-scoped
  `PlayerEvent::TrackEnded` (`PlayerEvent::ended`,
  `src/audio/mod.rs:116,138`) so queue advance/repeat fires
  (`src/ui/window.rs:3257-3274`). A per-generation completion record
  collapses a duplicate EOS — a second EOS publishes nothing. If
  the generation was superseded (output switch, new load) or
  cancelled before EOS completes, the completion is dropped: no
  `TrackEnded`, and no restoration against the new owner, guarded by
  the same generation identity the publisher already applies.
  Cancellation and terminal failure must never publish `TrackEnded`
  — a cancelled load ends as `Stopped` (§9.5) and a failed one as
  `PlayerEvent::Error` + `Stopped` (§9.4). Without this exit a
  finite track reaches pipeline EOS and simply stalls on the
  current item, because the UI advances only on `TrackEnded`.
  **The pump also owns the track duration:** it issues a
  TIME-format duration query against its own pipeline — re-asking
  until the demuxer patches a known value — and publishes the result
  into the session's §4.1 observation cache as `duration_ms`. It
  must, because the daemon cannot know the length of a pipe-fed
  item: the pipe carries raw PCM and never the prepared URI
  (`src/inputs/pipe.c`), so the daemon's JSON `item_length_ms` reads
  0/unknown until EOF, and a finite track would render LIVE with a
  pinned, disabled slider for its whole length
  (`src/ui/window.rs:3243-3253`). The JSON value is therefore a
  cross-check, never the source (§7); a stream whose duration the
  pipeline genuinely cannot answer keeps `duration_ms: None` and the
  honest LIVE rendering. This reuses the GStreamer decoder stack
  Tributary already requires — no new dependency — and it keeps the
  protected loopback ticket URI entirely inside Tributary's process:
  the daemon never receives the URL, only the decoded bytes.
- **Transport out:** OwnTone's AirPlay outputs, classic RAOP *and*
  AirPlay 2 (§5.4), discovered and paired by the daemon itself —
  including the password and PIN-verification flows Tributary cannot
  see from `host:port` alone (§3).
- **Receiver selection:** OwnTone's JSON API (`/api/outputs`) selects
  which output(s) receive the stream; the adapter maps its selected
  receiver onto the daemon's device list by the retained discovery
  device identifier — the normalized device MAC/`deviceid` that
  OwnTone itself parses into its output `id`
  (`src/outputs/raop.c:4220,4272`; `src/outputs/airplay.c:3945-3957,4004`)
  — at open time and re-checks it. The match is on the parsed
  numeric id: OwnTone parses the hex MAC to a `u64` and the API
  renders that `u64` as a decimal string, so a raw-string compare
  would miss. The display name is not the key:
  two receivers can share one, and `/api/outputs` exposes the output
  `id` and `name` but no receiver host/port or address (pinned
  `docs/json-api.md`, §Get a list of available outputs), so a
  name-only map — or a discovery record that had its `MAC@`/TXT
  identity stripped (§3) — would fail to disambiguate and could
  stream to the wrong receiver. A selected receiver with no retained
  identifier, or an identifier that does not resolve to exactly one
  output, fails closed with localized actionable guidance before any
  state is read or mutated, so a receiver that vanished or renamed
  also fails loudly. The enabled
  set is server-wide — `PUT /api/outputs/set` "enables all outputs
  with the given ids and disables the remaining outputs" — so
  isolating one receiver reconfigures the whole instance; the
  ownership and restoration rules below are what make that safe.
- **Lifecycle — a dedicated, Tributary-owned daemon instance.** The
  adapter requires an OwnTone instance of its own: spawned and
  supervised by Tributary (or a documented per-user service the
  implementation record installs), with its own config, cache and
  database directories, and its own loopback JSON API port. A normal
  shared system service is **not** an acceptable target. The reason
  is isolation, and the JSON API provides none
  ([`docs/json-api.md`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/docs/json-api.md),
  pinned 29.3): there is exactly **one player** (`PUT /api/player/*`
  drives the server's single playback state) and **one current
  queue** (`/api/queue`), so any other client — the user's own web
  UI, a mobile remote, another integration — can take either at any
  moment. A dedicated instance removes that contention by
  construction. "Daemon unreachable / version too old / pipe missing
  / API port taken" remain probe-time failures with actionable
  guidance. Ownership of the instance is verified, not assumed:
  `probe` and `open_session` confirm out of band — against the
  adapter's own supervision/installation record and the instance's
  state directory, the same trust domain as the lock file below —
  that the answering daemon is the dedicated Tributary-owned
  instance, and fail closed before reading or mutating any receiver
  state. The JSON API cannot make that distinction: it exposes no
  instance identity (`GET /api/config` returns only `version`,
  `websocket_port` and `buildoptions` — pinned
  [`docs/json-api.md`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/docs/json-api.md),
  §Server info), so a shared instance that merely looks healthy can
  never pass.
- **Exclusivity is locked before the first state read, revalidated,
  and restored — never assumed.** A dedicated instance is the default
  posture, not a substitute for the adapter treating the daemon as
  shared state, because the API offers no session boundary an adapter
  could lean on — any client can interleave between two HTTP calls,
  so sequencing checks without a lock would leave a window where
  another client takes over after our verification but before our
  takeover. Before its first state read, therefore, the session
  acquires an OS-level lock on the dedicated instance: an advisory
  `flock` (or the platform equivalent) on a lock file inside the
  instance's own state directory — a file only this adapter and the
  instance's supervision ever open. The lock is held for the
  session's entire lifetime: through recording the current enabled
  output set, verifying the player is stopped with an empty (or
  already-ours) queue, taking control (enable exactly the selected
  receiver, clear the queue, start our pipe item), every §4.4
  revalidation, and close-time restoration. A second Tributary
  session that cannot take the lock refuses immediately with the
  existing actionable-guidance path instead of interleaving with the
  holder. The lock is released only after restoration completes —
  player stopped, our queue items removed, the recorded enabled set
  re-applied. A cancelled open (§4.1 `OpenOutcome::Cancelled`)
  unwinds through exactly this path: steps already taken are
  recorded and reversed in order, and the `Cancelled` outcome is
  returned only after restoration completes — never as a shortcut
  past it (§9.5). Because a mutating daemon RPC that has already
  been transmitted lands on the daemon whether or not the client
  reads its response, restoration is not complete until the server
  side is quiesced: before returning `Cancelled` the unwinding call
  waits, bounded by the cleanup deadline, for acknowledgement that
  every transmitted mutating RPC has settled, or re-runs the
  reversal after the last in-flight one settles, so no mutation
  from the cancelled generation can land after the lock is
  released. A quiescence wait that misses the deadline is
  `Failed(SenderError::RecoveryPending)`, never `Cancelled`, and the
  incomplete-takeover record stays in place rather than reporting a
  clean unwind the adapter cannot prove; the resulting
  recovery-pending state defers every teardown/restoration claim
  until the request settles or the daemon is restarted. Because a request left
  unacknowledged at the deadline can still land afterwards, the
  adapter does not hand ownership on as mutation-free: it keeps
  recovery serialized until the outstanding request settles, or
  terminates/restarts the dedicated daemon to cancel it, and only
  then does restoration run — so neither the supervisor nor a next
  opener can interleave a restoration with the unsettled request.
  The load path then revokes its own media ticket
  via `revoke_if_current` (§4.1, §9.5) — immediately for a clean
  `Cancelled` return, and for the `RecoveryPending` branch only after
  the `RecoveryCompletion` handle resolves, where recovery has
  not reached a terminal outcome yet and the ticket was first moved
  out of the proxy's active lease into recovery custody as part of the
  transition that produced `RecoveryPending` (under the proxy's state
  lock, atomic with that transition, so no replacement load can revoke
  it early) — so the unwound load leaves no receiver
  session, no enabled-output change, and, once recovery reaches its
  terminal outcome, no live loopback route behind. A crashed holder releases the lock by OS semantics,
  and what happens next is defined, not incidental: before the
  first mutating step (the first output, queue, or player change),
  the session persists an incomplete-takeover record next to the
  lock file — the pre-takeover enabled-output set, which queue
  items are ours, and which takeover steps already ran. The record
  survives the crash, so the supervisor — or the next opener,
  before its own takeover — detects it and runs recovery. **OS lock
  release alone does not quiesce a request the dead holder already
  transmitted:** OwnTone may still apply an in-flight `outputs/set`,
  `queue/add`, or `player/play` after the OS-released lock lets the
  acquirer restore, and the persisted step record cannot serialize a
  request the server is already running, so a restore-first recovery
  could be overwritten by the dead holder's late mutation or
  interleave with the next opener. Recovery therefore quiesces the
  old daemon *first* — most reliably by terminating and restarting
  the dedicated instance, which is exactly what §4.3's owned,
  Tributary-spawned daemon makes safe, since a restart drops every
  connection and any in-flight request — or otherwise proves every
  request from the dead holder has settled. Only then does it stop the
  player if it is playing, remove Tributary-owned queue items,
  re-apply the recorded enabled set, verify, and remove
  the record and admit the new session. If a restoration step
  fails, the acquirer refuses with its own localized actionable
  error and leaves the record in place for the supervisor to
  retry; a half-taken-over daemon is never adopted silently.
  Recovery is bounded by its own documented recovery deadline (a
  named constant in the implementation record, distinct from the
  cleanup deadline): a restoration step that fails, or a serialized
  recovery whose window expires before restoration completes, is
  terminal for the `RecoveryCompletion` handle — rather than
  leaving its waiter pending forever, the adapter resolves it as
  `RecoveryOutcome::RestorationFailed` and leaves the record for
  the supervisor. Revocation is safe on that path because quiescence
  always precedes restoration and is itself bounded by the adapter's
  restart fallback — terminating and restarting the dedicated
  instance drops every connection and cancels any in-flight request —
  so no request that referenced the route survives recovery, and
  the retained record's recovery does not depend on the load path's
  loopback ticket; the load path may therefore stop waiting and
  revoke its own route at that outcome. Until that terminal outcome,
  explicit Stop or output destruction must not revoke the custodied
  route: it either drives the adapter's quiescence
  (terminate/restart) so this outcome is reached promptly, or
  transfers custody to this recovery owner, which revokes once the
  outcome is terminal. No reader of the handle is
  ever left without a terminal disposition. The
  §4.4 revalidation remains the backstop for external clients
  the lock cannot see. Open-time behavior is otherwise unchanged:
  refusing with actionable guidance if the player is active, and
  never preempting audible playback. A revalidation mismatch is
  session loss surfaced through the §9.4 contract, not a silent
  re-takeover. Should a future record ever attach to a pre-existing
  shared instance instead, it inherits every one of these — lock,
  revalidation, restoration — as hard, tested requirements plus a
  written justification; that is the exception path, not the
  default.

### 4.4 What must NOT change in this refactor

- **The fail-closed ordering:** the sender gate (`probe`) runs before
  the app-owned exact-route proxy mints a loopback ticket for the URI.
  `protected_load_fails_closed_before_any_pipeline_sees_the_secret`
  (`src/audio/airplay_output.rs:783-838`) must still pass without
  modification.
- **Position/duration evidence** keeps the same 500 ms
  generation-scoped publication cadence and event shape
  (`src/audio/airplay_output.rs:384-429`;
  [`docs/playback-history.md`](playback-history.md) pins the
  contract), but the timer is the **publisher, not the only source**:
  sessions expose their measurement through the nonblocking
  `SenderSession::observe` cache (§4.1), maintained by each adapter's
  own task — GStreamer adapters sample pipeline state there; the
  daemon adapter samples `item_progress_ms` for position and sources
  duration from its own decode pump, treating JSON `item_length_ms`
  as a cross-check only (§4.3, §7). Paused sessions freeze,
  disconnected ones set `stale` instead of extrapolating, and
  snapshots from a foreign generation are dropped, per §4.1.
- **Localization:** the honest unavailable message stays user-visible
  in every catalog; renaming away from the `raopsink` identifier is
  acceptable only once the selected replacement actually ships
  (the existing tests at `src/audio/airplay_output.rs:721-747` pin
  the message contents).
- **No silent fallback, ever (the P2.9 lesson):** a probe failure is a
  hard, localized error. No adapter may fall back to another adapter,
  a subprocess that "might work", or an unrelated output. Selection
  is configuration, failure is explicit.

## 5. Sender candidates (maintained universe, 2026-09)

### 5.1 GStreamer `raopsink` — historical/unmaintained

Unchanged from the 2026-07-20 review: removed from gst-plugins-bad
upstream after remaining unported; the historical `apexsink` embedded
only an RSA public modulus/exponent used to encrypt a generated
outbound session key; no official GStreamer, Homebrew, or MSYS2
package ships a `raopsink`
([`docs/release-component-policy.md:87-96`](release-component-policy.md)).
**Non-option as a dependency; retained only as the adapter around a
user-supplied element** (§4.2), because the code and tests for that
gate already exist.

### 5.2 `shairport-sync` — AirPlay receiver, not a sender

Correct as recorded in the P2.9 remediation: it advertises as a
receiver; piping PCM into it ignores the device the user selected.
Not a sender candidate. Note added by this revision: OwnTone's 29.1
changelog ("shairport-sync Airplay 2 mode via support for PTP timing")
is about *sending to* shairport-sync receivers, which confirms
shairport-sync's role on the receiving end.

### 5.3 `libshairplay` / `libraop` and forks — receiver libraries

Unmaintained receiver-side libraries reverse-engineered from AirPlay 1
traffic (their own documentation describes the receiving end). Their
RTP/AES session code documents the receiving end of §2.2. Not
candidates; no maintained sender builds on them.

### 5.4 OwnTone 29.3 — maintained daemon sender (the revision-1 omission)

[OwnTone](https://owntone.github.io/owntone-server/) is the maintained
successor of forked-daapd (renamed at 28.0), an GPL-2.0-or-later
audio server whose *primary* feature set is exactly the sender side
Tributary needs:

- **Actively maintained.** Release 29.3 shipped 2026-07-22 (six weeks
  before this revision); the 29.x series has had four releases in a
  year, with AirPlay fixes in each.
- **Classic RAOP sender:** the `raop.c` implementation §2 describes —
  352-sample framing, RSA/AES session keys, retransmission (a feature
  since forked-daapd 0.13), per-device quirks, timing and control
  ports, volume mapping, password and verification handling.
- **AirPlay 2 sender:** supported since 27.3 ("support for AirPlay 2
  speakers, incl. compressed ALAC"), with password authentication
  (28.5/28.6), PTP timing for the devices that need it (29.1), and
  AirPlay 2 now the default mode (29.1). This is the only maintained,
  packaged, open-source AirPlay 2 *sender* this investigation located.
- **PCM16 pipe input** (`src/inputs/pipe.c`) and a fifo output at
  `{44100, 16, 2}` (`src/outputs/fifo.c`) — the integration surface
  of §4.3.
- **JSON API** for output selection and volume, with a documented
  web/UI contract — the control surface of §4.3.
- **Packaging reality (primary source, installation docs):** upstream
  publishes Debian/Ubuntu amd64 packages, a Raspberry Pi OS apt
  repository, an official Docker image, OpenWrt (`opkg install
  owntone`), and FreeBSD (`pkg install owntone`). **OwnTone is not in
  the official Debian archives** (documented upstream: no Debian
  maintainer; web-UI policy). Any Tributary record that adopts it
  must therefore document per-platform acquisition the way the
  release-component policy treats all external dependencies: pinned
  or documented package sources, never an incidental download, and
  never a bundle inside Tributary's artifacts. These channels are
  OwnTone's, not Tributary's: §8 turns them into an explicit
  availability decision per actual Tributary package target, and
  most Tributary targets have none.
- **Licensing:** GPL-2.0-or-later. The §4.3 design *intends* a
  process boundary: OwnTone would run as its own program, integrated
  through a FIFO and an HTTP JSON API, with no linking and no
  bundling. That architecture is a necessary input to a "no combined
  work" conclusion — it is not the conclusion. Whether the GPL
  obligations in fact stay entirely upstream is a fact-specific
  design-and-distribution question (what ships in Tributary's
  artifacts, from which sources, with which install instructions)
  that this investigation surveys but does not adjudicate. The
  implementation record must therefore complete and record that
  review before any sender code lands: it confirms distributor and
  distribution mode for the daemon (upstream, via the §5.4
  channels), that Tributary ships no OwnTone code or binaries, that
  install documentation points at upstream channels, and — only
  after that record exists — may state the boundary conclusions this
  paragraph sketches. Until then this document claims the mechanics,
  not the legal result: no bundling, no linking, no embedded key
  material (§2.2), and an open review obligation. The policy's
  review-boundary section governs *bundled* components and embedded
  key material; neither occurs on this path (§2.2), which is what
  keeps this path reviewable without an exception — it does not
  itself settle the combined-work question.

**This is the candidate revision 1 should have found.** It answers
every acceptance dimension the task sets: maintained (29.3),
AirPlay 2-capable, pairing/password/verification handled by the
daemon, no key material in Tributary, no bundling.

### 5.5 PipeWire `module-raop-sink` — maintained, desktop-stack-bound

PipeWire ships a maintained AirPlay 1 sink module:
`raop.encryption.type` of "none", "RSA" or "auth_setup", an optional
`raop.password`, ALAC, and the same 352-frame packetization
([module-raop-sink.c](https://github.com/PipeWire/pipewire/blob/b741e0c74f5436f0c925f7741140db0efd32cf4e/src/modules/module-raop-sink.c)).
MIT-licensed, actively maintained — but it exists inside the desktop
audio graph: it creates a PipeWire sink that streams to one fixed
RAOP endpoint, discovered by the companion `module-raop-discover`.
As a candidate: viable only as "let the user's audio stack own
AirPlay" — Tributary would output to local PipeWire and the *user*
selects the AirPlay sink in their desktop tools. That bypasses
Tributary's output selector rather than implementing it, and covers
AirPlay 1 only. **Verdict:** document as the escape hatch it is; not
the selected path. No PipeWire module ships an AirPlay 2 sender that
an application could target per-device (as of master, 2026-09).

### 5.6 Tributary-owned RAOP-1 sender

A small Rust RAOP client implementing §2.2: plaintext RTSP +
ANNOUNCE SDP (`a=rsaaeskey`/`a=aesiv`), 352-sample ALAC framing
(§2.4 — including the `avenc_alac` framing constraint recorded
there), RTP audio/control/timing, retransmission, volume
SET_PARAMETER, MD5 password auth, and the verification-PIN flow.
Runs as a `SenderSession` behind the §4.1 contract (in-process or
subprocess — the seam supports both; §10.4 defers this path until the
daemon recommendation fails validation).

- **Cost drivers:** the embedded well-known RSA public modulus
  (§2.2) triggers the dedicated release-component review
  ([`docs/release-component-policy.md:82-84`](release-component-policy.md))
  — update the shared policy, tests, changelog, and this document
  together, with artifact evidence; the ALAC framing fix; real-device
  matrices for the `et=3/4` devices that cannot use plain RSA/AES
  (those need MFi-SAP, which is out of scope and would make those
  devices explicitly unsupported with a clear error).
- **Scope honestly:** this is the multi-month option. It buys a
  no-daemon dependency and nothing else that §5.4 doesn't already
  provide, and it ships AirPlay 1 only.

### 5.7 AirPlay 2 sender from scratch — deferred

After §2.3 the scope is clear: pair-setup/pair-verify (SRP6a, Ed25519,
X25519), ChaCha20-Poly1305 control and audio, PTP timing. Every
maintained implementation of that stack lives inside OwnTone (as the
sender) and receiver projects. A from-scratch Tributary sender
duplicates multi-year protocol work that §5.4 already ships. **Out of
scope; revisit only if the OwnTone path fails in validation.**

## 6. Recommendation

**Adopt the OwnTone 29.3 process adapter (§4.3, §5.4) as the first
shipping path, behind the §4.1 seam, with the `raopsink` adapter (§4.2)
retained for user-supplied elements — probe-gated independently of
packaging. The §8 availability matrix gates the OwnTone adapter's
acquisition only; it never restricts the user-supplied-element path,
which stays available wherever `probe` finds a working `raopsink`
(§4.2, §5.1).** The matrix is decided
per actual Tributary package target (install matrix,
[`README.md`](../README.md)) against documented OwnTone acquisition
and the §4.3 dedicated-instance runtime path — never inherited from
OwnTone's own channel list, most of which (Docker, OpenWrt, FreeBSD)
serves platforms where Tributary ships nothing. Today the matrix
marks the `.deb` target on Debian/Ubuntu **amd64** available; the
arm64 `.deb` Tributary also publishes has no documented OwnTone
acquisition and ships fail-closed (§8). Fedora RPM/COPR, Arch AUR,
Flatpak, macOS `.dmg`, and Windows
(winget/installer/zip) ship the fail-closed unavailable state for the
OwnTone adapter — the localized guidance names the platform
limitation — and confer no OwnTone sender behavior; a user-supplied
`raopsink` element keeps its independent §4.2 probe-gated path
there. Sender support on the unavailable targets requires a future
supported acquisition/integration path, which this investigation
deliberately does not promise (§8, §9).

Ordering rationale:

1. It is the only candidate that is maintained, packaged, and reaches
   both AirPlay 1 *and* AirPlay 2 receivers (§5.4).
2. It embeds no key material and bundles nothing, so it needs no
   review-boundary exception — only the dependency-documentation
   discipline the policy already requires (§5.4 packaging note).
3. It moves pairing, password, PIN verification, retransmission, and
   timing off Tributary's plate, which is where the revision-1
   implicit plan (§5.6) would have cost months.
4. The seam refactor (§4) is small, mechanical, and test-preserving;
   the daemon adapter is additive.

The Tributary-owned RAOP-1 sender (§5.6) remains documented as the
no-daemon fallback, pending its dedicated key-provenance review.
AirPlay 2 in-tree (§5.7) is rejected for now. The P2.4-C index entry
([`docs/task.md:399-408`](task.md)) points at this document and
remains unchecked "In flight" until the design is accepted; the
follow-on implementation record must restate its
choice and the policy call-back before writing sender code.

## 7. Pairing, encrypted control, audio, timing — per selected path

What the implementation record must nail down, per §4.3:

- **Pairing/verification:** owned by the daemon. Tributary surfaces
  the daemon's states — device needs password (config), device needs
  PIN verification — as localized, actionable messages. The JSON API
  pairing/verification flows are triggered from Tributary's settings
  surface, not silently.
- **Encrypted control:** end to end inside the daemon (§2.2/§2.3);
  the Tributary↔daemon leg is a local FIFO plus loopback HTTP and
  needs no additional cryptography. The JSON API listener must be
  bound to loopback only in the adapter's generated config.
  **Loopback is network isolation, not authentication, and the
  design says so explicitly.** OwnTone's JSON API has no native
  authentication (pinned [`docs/json-api.md`](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/docs/json-api.md)
  documents none; access control is a deployment concern), and its
  endpoints mutate state — `PUT /api/outputs/set` rewrites the
  enabled-output set, `/api/queue/clear` empties the queue, and the
  `/api/player/*` endpoints drive playback. The threat model is
  therefore explicit and recorded here: the dedicated instance is
  owned by the invoking user; the adapter generates its config,
  FIFO, lock file, and state directories with owner-only
  permissions (umask-enforced, verified in the implementation
  record's tests); and **every local process running as the same
  user is trusted by this design** — same-user interference is
  accepted residual risk, and cross-user interference is excluded
  by OS user separation, not by the API. A record that must relax
  same-user trust (shared multi-user hosts) has to add an
  authentication or reverse-proxy layer and re-open this section;
  this one documents the trust boundary instead of pretending
  loopback binding authenticates.
- **Audio:** s16le 44100 Hz stereo into the pipe (§2.4, §4.3);
  encoding, framing, and per-device quirks are the daemon's.
- **Timing/position:** the daemon session publishes position and
  duration through the §4.1 observation cache, but the two values do
  not share a source: position is sampled from the JSON API player
  progress (`item_progress_ms`), while duration comes from the
  adapter's own decode pipeline (§4.3 pump duration query). The JSON
  `item_length_ms` is a fallback/cross-check only and is never
  authoritative for pipe-fed items, which receive no length over the
  pipe and read 0/unknown until EOF — trusting it would render a
  finite track as LIVE with a pinned, disabled slider
  (`src/ui/window.rs:3243-3253`). The 500 ms publication cadence is
  unchanged (§4.4), and the snapshot semantics are preserved: paused
  sessions freeze position, stale snapshots stop extrapolation, and
  foreign-generation snapshots are dropped by the publisher (§4.1).
  Receiver latency is invisible to the UI, as today — documented,
  not hidden.
- **Multi-room:** out of scope unless separately approved (task.md
  P2.4); the adapter enables exactly the one discovered device the
  user activated — resolved to the daemon's output by the retained
  device identifier, never by display name (§4.3) — which, given the
  server-wide enabled-output set
  (§4.3), means the dedicated instance streams to that receiver
  alone, and close or failure restores the set recorded at takeover.

## 8. Packaging consequence

- **No bundling.** OwnTone arrives from its own documented sources
  (§5.4). Tributary's artifacts gain no new files, so the
  forbidden-bundled-components audit is a *positive confirmation*
  only; the full shared-policy containment pipeline (Windows ZIP/PE,
  macOS, native Linux, Flatpak gates) still runs on the
  implementation PR and records artifact evidence, per
  [`docs/release-component-policy.md`](release-component-policy.md).
- **Dependency documentation, decided per actual Tributary package
  target — never per OwnTone channel.** Tributary ships: Fedora RPM
  (COPR), `.rpm` and `.deb` release artifacts, Arch AUR packages
  (`tributary`, `tributary-bin`, `tributary-git`), Flatpak, macOS
  `.dmg`, and Windows (winget, Inno installer, zip)
  ([`README.md`](../README.md) install section). OwnTone's documented
  acquisition channels (§5.4, rechecked 2026-09-09: Raspberry Pi OS,
  Debian/Ubuntu amd64, Docker, OpenWrt, FreeBSD) are upstream's, not
  Tributary platforms — Docker, OpenWrt, and FreeBSD are not Tributary
  package targets and confer no availability here. The design's
  availability decision for every Tributary target follows; each
  entry was checked against the documented channel on 2026-09-09:

  - `.deb` on Debian/Ubuntu **amd64** — **Available.** Upstream
    documents Debian/Ubuntu amd64 packages; the user-native daemon is
    one Tributary can own and supervise (§4.3).
  - `.deb` on Debian/Ubuntu **arm64** — **Unavailable —
    fail-closed.** Tributary's release workflow publishes
    `tributary-arm64.deb`, but the documented OwnTone channels above
    cover amd64 only on this package's targets: no upstream Debian/
    Ubuntu arm64 package is documented. Upstream's Raspberry Pi OS
    apt repository is recorded honestly (§5.4) but is not treated as
    acquisition for generic arm64 Debian/Ubuntu installs — same
    discipline as the Arch AUR entry below — so absent documented
    acquisition this target ships the fail-closed unavailable state
    until an evidence-backed arm64 channel is recorded.
  - Fedora RPM (COPR, `.rpm` releases) — **Unavailable —
    fail-closed.** No OwnTone channel is documented upstream.
  - Arch AUR (`tributary`, `tributary-bin`, `tributary-git`) —
    **Unavailable — fail-closed.** Only the community AUR
    `owntone-server` package exists; not an upstream-documented
    channel.
  - Flatpak — **Unavailable — fail-closed.** No OwnTone app or
    runtime extension on Flathub; the sandbox cannot own a host
    daemon (§4.3); bundling forbidden (§11).
  - macOS `.dmg` — **Unavailable — fail-closed.** No channel
    documented upstream (no Homebrew formula); bundling forbidden
    (§11).
  - Windows (winget, installer, zip) — **Unavailable — fail-closed.**
    No channel documented upstream; bundling forbidden (§11).

  Targets marked available gain the "for AirPlay output, install
  OwnTone ≥ 29.x" install-docs entry with the pinned source (upstream
  releases page); where OwnTone is absent even from a covered
  distro's own archives (no official Debian archive), the docs say so
  and the probe error repeats it. The arm64 `.deb` install entry
  names the absent arm64 acquisition explicitly, mirroring the
  fail-closed row above. Unavailable targets ship the honest
  fail-closed state: install docs state that AirPlay output requires
  an OwnTone-capable target with no acquisition path implied, and the
  probe fails closed with the same localized unavailable contract as
  today's `raopsink` message. The Arch entry names the community AUR
  `owntone-server` package honestly but does not treat it as a
  supported acquisition path, because it fails the
  dependency-documentation discipline above (community-maintained,
  not upstream-documented, no pinned channel). This matrix — including
  its amd64/arm64 split — gates the OwnTone adapter's acquisition
  only; the user-supplied `raopsink` adapter (§4.2, §5.1) stays
  independently probe-gated on every target. The acceptance matrix
  (§9) scopes to match.
- **Probe reflects reality:** `AirplaySender::probe` for the daemon
  adapter checks: binary/service present (documented discovery only —
  no PATH guessing beyond the documented locations), daemon
  reachable, API version compatible, pipe creatable, and — before
  any player, queue, or output state is read — that the answering
  daemon is the dedicated Tributary-owned instance of §4.3,
  verified out of band against the adapter's
  supervision/installation record and the instance's state
  directory. The JSON API exposes no instance identity
  (`GET /api/config` returns only `version`, `websocket_port` and
  `buildoptions` — pinned `docs/json-api.md` §Server info), so a
  shared instance — or any daemon the record does not own — can
  never pass by looking healthy; it fails closed. Each failure
  mode has its own localized message.

## 9. Real-device tests and acceptance

The existing suite
([`src/audio/airplay_output.rs:717-838`](../src/audio/airplay_output.rs))
pins the absence path with high specificity. The implementation
record for the selected path must add, at minimum:

1. **Probe-failure regression** (mirrors
   `a_missing_raopsink_is_refused_with_honest_guidance`): when the
   selected adapter cannot probe, the load is refused before any
   per-track proxy work, regardless of configuration.
2. **Adapter-injection stub** (mirrors
   `a_missing_raopsink_load_fails_loudly_not_silently`): a stub
   `AirplaySender` returning
   `OpenOutcome::Failed(SenderError::Dependency(..))`
   makes `finish_load` emit the
   generation-tagged `PlayerEvent::Error` followed by `Stopped`.
3. **Registry-attribute regression** for the GStreamer adapter: the
   `find_feature` lookup remains the source of truth; a
   pretending/fake factory must still trip `probe`.
4. **Reconnect acceptance:** with the receiver restarted mid-track
   (or the daemon's session dropped, or a §4.3 revalidation mismatch
   detected — the enabled set no longer selects exactly our receiver,
   or foreign player/queue activity appears), the failure is surfaced
   within the event contract, the loopback ticket is revoked
   (`revoke_if_current` ordering, as in today's bus watch
   `src/audio/airplay_output.rs:325-352`), and an explicit re-load
   recovers — no zombie session, no silent stall, no automatic
   reconnect storm, no silent re-takeover.
5. **Cancellation acceptance:** stopping or switching output
   mid-stream tears the receiver session down in order (audio path
   stopped before the loopback route is invalidated — the ordering
   bug class `close_session`'s doc comment warns about,
   `src/audio/airplay_output.rs:293-305`), revokes its own media
   ticket via `revoke_if_current` (never a newer replacement's),
   leaving zero retained routes or resources for the cancelled
   load, leaves no receiver-side playback continuing, and returns
   `Stopped` for the exact load generation; on the daemon adapter
   it also restores the state recorded at takeover — player
   stopped, our queue items removed, the recorded enabled-output
   set re-applied (§4.3). The same acceptance covers cancellation
   *during* negotiation, including Stop or output replacement
   landing while a blocking operation (RTSP handshake, daemon RPC,
   FIFO/API setup) is in flight: the operation is aborted rather
   than awaited, and the load whose `OpenCancel` was set yields
   `OpenOutcome::Cancelled` only after the restoration path
   completes **and** every mutating daemon RPC already transmitted
   has been quiesced — acknowledged settled within the cleanup
   deadline, or followed by a compensating restoration re-run after
   the last in-flight one settles — so a late server-side effect
   cannot land after the teardown. Discarding a late client response
   is not by itself sufficient: the test holds a mutating RPC
   (`outputs/set`, `queue/add`, or `player/play`) in flight across
   the cancel and asserts the daemon state it produced is unwound
    before `Cancelled` is returned. A mutating RPC that neither
    settles nor acknowledges within the deadline yields
    `OpenOutcome::Failed(SenderError::RecoveryPending)`, never
    `Cancelled` and never a plain `Deadline`, and leaves the
    incomplete-takeover record in place. That timeout
    branch must not be asserted mutation-free and must not be
    asserted to have completed teardown: the test holds the
    request unsettled past the deadline, asserts the outcome is the
    recovery-pending failure state, and asserts the adapter keeps
    recovery serialized until it settles — or terminates/restarts the
    dedicated daemon to cancel it — before releasing ownership and
    before any restoration is claimed, and
    that a supervisor or next opener cannot interleave a restoration
    with the unsettled request. The result is no user-facing error
    for a genuinely cancelled load, no error event for the cancelled
    generation, no half-taken-over daemon adopted while a request is
    unsettled, and a following load opens cleanly once the outstanding
    request has settled or been cancelled (§4.1, §4.3). The same
    acceptance asserts the ticket ordering across both branches: with
    the outcome `Cancelled` (restoration complete) the load path
    revokes its own media ticket via `revoke_if_current` on receipt;
    with `Failed(SenderError::RecoveryPending)` (recovery still
    pending) it does **not** revoke on receipt — the test observes the
    loopback route still live while recovery is outstanding and
    asserts revocation happens only after the carried
    `RecoveryCompletion` resolves. That assertion is
    **outcome-sensitive**: on `Restored` the revocation follows
    completed restoration, while on `RestorationFailed` — a failed
    restoration step, or the recovery deadline passing with no
    restoration attempt — it follows only the documented
    settle-or-restart quiescence that already guarantees no request
    referencing the route survives (item 13's deadline variant is the
    same input, not a contradiction). A
    load whose ticket was superseded while awaiting completion must
    not revoke the newer replacement's ticket (the `revoke_if_current`
    identity check). A targeted
    interposition test races `cancel` against an operation the test
    holds open and asserts the abort happens before that operation
    would have returned.
6. **Authentication-failure acceptance:** a password-protected
   receiver with no configured password, and a wrong-password case,
   each surface a distinct, localized, actionable error (pointing at
   the configuration surface), with no retry loop; a
   verification-PIN-pending receiver surfaces the daemon's pending
   state instead of a generic failure.
7. **Framing conformance (GStreamer and in-tree paths only):** the
   encoded stream honors the 352-sample framing announced to
   receivers (§2.4), verified against the encoder configuration or
   re-framer in a unit test, since a mis-framed stream manifests only
   as device-specific glitches.
8. **Real-device integration**, gated behind
   `AIRPLAY_TEST_RECEIVER` (opt-in, never CI-default): one track
   through the selected adapter against a reachable receiver —
   covering play/pause/stop, volume, and the §9.4-§9.6 paths on real
   hardware. AirPlay 2 validation additionally requires a real AP2
   receiver (HomePod/Apple TV class) for any record that flips the
   §3 discovery filter.
9. **Process-death acceptance:** the lock holder is killed
   mid-playback (SIGKILL, no cleanup path) after takeover, including
   with a mutating request (`outputs/set`, `queue/add`, or
   `player/play`) already transmitted and in flight; the
   supervisor or the next opener detects the persisted
   incomplete-takeover record (§4.3) and, **before restoring or
   admitting the new session**, quiesces the dead holder — terminating
   and restarting the dedicated instance, or proving every request
   from the dead holder has settled — because OS lock release alone
   cannot retract a request OwnTone is already applying. Only then
   does it restore the daemon — player stopped, our queue items
   removed, the recorded enabled-output set re-applied — and admit
   the new session. The test holds a mutating request from the killed
   holder in flight, completes recovery, and asserts no late mutation
   from the dead generation lands after the restore. A
   restoration failure refuses the new session with localized
   guidance instead of proceeding over a half-taken-over daemon.
10. **Natural-completion acceptance:** a finite track played
    through the daemon adapter reaches pipeline EOS without a
    terminal write outcome; the adapter drains the decoded
    remainder, closes the pipe write end so the daemon sees
    end-of-input, and then waits, bounded by a drain deadline, for
    daemon-confirmed item completion (polling the daemon's
    player/item state) before it disposes of its
    pipe/queue/session resources, restores the dedicated daemon to
    the state recorded at takeover, and publishes exactly one
    generation-scoped `TrackEnded` so the queue advances (or
    repeats) instead of stalling on the finished item
    (`src/ui/window.rs:3257-3274`). The test asserts that a final
    accepted `write_pcm` alone does not trigger disposal or
    `TrackEnded`, and that a drain deadline miss or transport loss
    during the wait ends as failure (`Error` + `Stopped`) and
    publishes no `TrackEnded`. It covers normal completion, a
    duplicate EOS (still exactly one `TrackEnded`), and a
    superseded generation (EOS arriving after the load was replaced
    or cancelled publishes nothing and mutates nothing).
    Cancellation and terminal failure are asserted *not* to publish
    `TrackEnded` — they end as `Stopped`/`Error`+`Stopped` per
    §9.4/§9.5.
11. **Receiver-identity mapping acceptance:** with two receivers
    advertising the same display name and different device MACs, the
    adapter enables exactly the output whose OwnTone `/api/outputs`
    `id` equals the retained discovery identifier of the selected
    receiver — never a name-match — and asserts the enabled `id` set
    read back from `/api/outputs`. A selected receiver whose retained
    identifier is missing or does not resolve to exactly one output is
    refused with localized guidance before any mutating call, so no
    output is enabled and no receiver state is touched. The same
    acceptance covers the selector the user actually drives: with two
    same-named receivers discovered, both appear as distinct selector
    rows (the display-name dedup of `is_device_in_output_list`,
    `src/ui/discovery_handler.rs:286`, :439-459, is replaced by the
    retained identifier), each row keeps its identifier through
    activation, and selecting the second receiver streams to the
    second device — so the mapping is exercised through the real UI
    flow, not only the adapter API, and the second same-named receiver
    is never silently unselectable.
12. **Superseded-ticket acceptance:** a replacement load's media
    preparation lands while a `Failed(SenderError::RecoveryPending)`
    recovery is in flight. The test holds the mutating RPC unsettled
    (or the restoration step) so recovery is still pending, then
    starts the replacement load through `prepare_with_server_start`
    (`src/audio/gstreamer_media.rs:255-271`) and asserts the pending
    load's loopback route is **not** revoked: because that supersession
    path takes and unconditionally revokes the active lease, the test
    proves the ticket had already been moved off the active lease into
    recovery custody **as part of the transition that produced
    `RecoveryPending`** — not by the load path after receipt — and is
    therefore absent from the active lease the replacement takes. The
    replacement arrives *after* the recovery outcome is observable, so
    the test exercises the window the atomic hand-off must close, not
    just the steady state. It then releases recovery, asserts the old
    route is revoked only after the carried `RecoveryCompletion`
    reaches its terminal outcome, and asserts the replacement's own
    ticket was never touched (the identity check). It also asserts an
    explicit Stop during the same window does **not** revoke the
    custodied route while recovery is unresolved: the Stop must either
    drive recovery to its terminal quiesced outcome
    (terminating/restarting the dedicated daemon) before the route is
    revoked, or leave the revocation to the recovery owner that owns
    custody.
13. **Persistent-restoration-failure acceptance:** a restoration step
    fails while a `RecoveryPending` recovery is serialized. The test
    asserts the incomplete-takeover record is retained for the
    supervisor (§4.3), that the carried `RecoveryCompletion` resolves
    terminally as `RecoveryOutcome::RestorationFailed` rather than
    leaving the waiter pending, that the load path then revokes its
    own loopback ticket and no route is stranded, and that the
    supervisor's later retry clears the retained record. A second
    variant holds recovery past the documented recovery deadline with
    no restoration attempt and asserts the same terminal outcome, so
    no reader of the handle can wait indefinitely; that variant is the
    input item 5 must treat as outcome-sensitive — revocation there
    follows only the documented quiescence prerequisite, because no
    restoration ran.

**Platform scope:** items 1-13 run on the package targets the §8
matrix marks available for the OwnTone adapter (today: the `.deb`
target on Debian/Ubuntu **amd64**). On every OwnTone-unavailable
target — the arm64 `.deb`, Fedora, Arch/AUR, Flatpak, macOS,
Windows — the acceptance contract for the OwnTone path is the
fail-closed path itself: §9.1's probe refusal with localized guidance
naming the platform limitation. No OwnTone sender playback is claimed
there until a supported acquisition path exists. The user-supplied
`raopsink` adapter (§4.2, §5.1) is not governed by this scope: it
remains independently probe-gated on every target, so where a user
supplies a working element, today's probe-then-open behavior and its
already-pinned tests (§1) are unchanged.

## 10. Proposed next-record plan

1. **Seam refactor (mechanical).** Land the §4.1 contract with the
   existing GStreamer path as the first `AirplaySender`; the contract
   carries the prepared URI exactly as `open_prepared_session` does
   today (`src/audio/airplay_output.rs:231-241`), so no behavior
   change; the §9.1-§9.3 tests land here. Locked `cargo check`,
   `cargo clippy` (debug + release), `cargo test --all-targets`.
2. **OwnTone daemon adapter.** Dependency documentation per §8,
   daemon lifecycle, JSON API integration, FIFO transport, §9.4-§9.6
   acceptance, real-device validation. Update
   `docs/release-component-policy.md`'s follow-on section to record
   the dependency decision (no exception required — document that
   conclusion explicitly).
3. **Discovery identifier retention (prerequisite of the daemon
   mapping in 2).** Retain the normalized device MAC/`deviceid` on
   `DiscoveredServer` and carry it through to the seam target; replace
   the UI output selector's display-name deduplication
   (`is_device_in_output_list`, `src/ui/discovery_handler.rs:286`,
   :439-459) with an identifier-based check and retain the identifier
   on each output row through activation, so a second same-named
   receiver stays selectable and reaches the mapping; then add the
   endpoint-to-output mapping before the daemon adapter may enable a
   receiver, so it never has to name-match (§3 consequences 3-4,
   §4.3, §9 item 11). **AirPlay-2
   enablement (separate, only after 2 validates):** extend
   `DiscoveredServer` for whatever else the daemon mapping needs
   (§3, consequence 2), flip the §3 discovery filter, add AP2
   real-device coverage.
4. **Only if validation fails:** fall back to §5.6 with its dedicated
   key-provenance review as a prerequisite.

## 11. What this investigation deliberately does not do

- It does not ship an embedded RAOP-1 sender library, and does not
  embed any key material in Tributary.
- It does not loosen
  [`build-aux/packaging/forbidden-bundled-components.txt`](../build-aux/packaging/forbidden-bundled-components.txt)
  or bundle OwnTone (or any daemon) into release artifacts.
- It does not implement AirPlay 2, MFi-SAP/FairPlay-encrypted session
  types (`et=3/4`), or multi-room sync.
- It does not promise a target date; the P2.1 feature focus leading
  the current active-backlog count
  ([`docs/task.md:31-33`](task.md) — **17/57 (29.8%)** implementation
  records complete, retained baseline **17/39**, kept synchronized
  with that file's literal counters) stays ahead of this work in the
  backlog order.
- Its only changes outside its own file are the two cross-references
  this branch already carries — the P2.4-C in-flight note in
  [`docs/task.md`](task.md) (checkbox intentionally unchanged; the
  item closes on an accepted design, not on this record) and the
  follow-on note in
  [`docs/release-component-policy.md`](release-component-policy.md)
  — plus the mechanical status-counter correction in
  `docs/task.md:31-33` (and the dated implementation-log entry that
  records it) so the synchronized count above is true.
  The substantive updates to both — the dependency decision, the
  shared-policy containment run, the changelog entry — belong to the
  implementation records that accept this design.
