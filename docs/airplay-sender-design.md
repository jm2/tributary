# AirPlay sender design investigation

Status: design record, no implementation in this bead.

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
- AirPlay-2-tagged rows are dropped at the UI boundary until a sender
  exists ([`src/ui/discovery_handler.rs:45-55`](../src/ui/discovery_handler.rs)).
- `requires_password` is `None` for every discovered AirPlay device;
  nothing populates it from `pw`.

Consequences for the design:

1. **The sender contract takes `{ display_name, host, port }` and
   nothing else.** Any protocol decision that needs `pw`, `et`, `sf`,
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
/// forever on a session that has already died.
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

/// A selectable transmission path. One immutable instance per
/// protocol/backend, chosen at load time by configuration — never
/// silently, never per-track.
trait AirplaySender: Send + Sync {
    fn name(&self) -> &'static str;
    /// `Ok(())` when this sender can transmit on this host. `Err`
    /// must carry the user-actionable guidance that the load path
    /// surfaces verbatim (this is today's localized
    /// `airplay_raopsink_missing` contract, generalized).
    ///
    /// Bounded by contract: `probe` performs only the documented
    /// discovery/health checks of §8 ("Probe reflects reality"),
    /// enforces the adapter's documented probe deadline (a named
    /// constant the implementation record states; the seam never
    /// leaves a load pending on an unbounded check), and surfaces a
    /// deadline overrun as its own error variant — never by
    /// blocking. It holds no receiver-session resources, so
    /// cancellation (the load being dropped or its generation
    /// superseded) needs no protocol cleanup: dropping the call in
    /// flight is the whole cleanup.
    fn probe(&self) -> Result<(), String>;
    /// Negotiate a session with the receiver at `host:port`, sourcing
    /// audio from the prepared media at `prepared_uri`, and return
    /// it. Called only after `probe` succeeded and after media
    /// preparation, so a failure here is a receiver-side failure, not
    /// a missing dependency. The seam must carry `prepared_uri`
    /// because a sender is one immutable instance per backend — never
    /// per track — so it cannot capture the URI anywhere else; it is
    /// the same loopback URL today's `open_prepared_session` passes
    /// to `build_raop_pipeline`
    /// (`src/audio/airplay_output.rs:231-241`). Ticket revocation on
    /// failure stays in the load path (`open_prepared_media`,
    /// :199-208), exactly as today.
    ///
    /// Bounded and cancellable by contract: the call enforces the
    /// adapter's documented open deadline (again a named constant
    /// the implementation record states), and cancellation — the
    /// load being dropped or its generation superseded mid-call —
    /// aborts any in-flight negotiation. On a deadline miss or a
    /// cancellation the implementation tears down everything it
    /// created so far (receiver session, queue items, enabled-output
    /// changes) through the same restoration path as failure (§4.3)
    /// before the error surfaces. The method never returns a
    /// half-open session, and a load can never remain pending on it
    /// indefinitely; after timeout the load fails with an explicit,
    /// localized deadline error (§9.1 contract).
    fn open_session(
        &self,
        target: &AirplayTarget,
        prepared_uri: &str,
        event_tx: async_channel::Sender<PlayerEvent>,
        generation: PlayerEventGeneration,
    ) -> Result<Box<dyn SenderSession>, String>;
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
- **`open_session` returns a session, not a sink element.** Pause /
  resume / volume / flush are protocol operations (RTSP
  SET_PARAMETER / PAUSE, daemon RPC), not pipeline state writes, so
  they belong to the session object.
- **Events stay generation-scoped.** Adapters receive the event
  channel and the load's generation so receiver-side failures
  (reconnect exhaustion, auth refusal) surface through the exact same
  `PlayerEvent::Error` + `Stopped` shape the tests pin today
  (§9).
- **A position/duration observation seam, not timer-only evidence.**
  `SenderSession::observe` returns the session's cached snapshot
  without I/O. Each adapter owns how the cache is produced — the
  GStreamer adapter samples pipeline state on a session-owned task,
  the daemon adapter samples the JSON API player progress
  (`item_progress_ms`/`item_length_ms`, §7) — while the seam owns the
  semantics: paused sessions freeze their position instead of
  advancing it, disconnected sessions set `stale` instead of
  extrapolating, and snapshots from a foreign generation are dropped
  by the publisher. The UI's 500 ms timer remains the *publisher*
  (§4.4); it reads the cache rather than being the only measurement.

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
  session. This reuses the GStreamer decoder stack
  Tributary already requires — no new dependency — and it keeps the
  protected loopback ticket URI entirely inside Tributary's process:
  the daemon never receives the URL, only the decoded bytes.
- **Transport out:** OwnTone's AirPlay outputs, classic RAOP *and*
  AirPlay 2 (§5.4), discovered and paired by the daemon itself —
  including the password and PIN-verification flows Tributary cannot
  see from `host:port` alone (§3).
- **Receiver selection:** OwnTone's JSON API (`/api/outputs`) selects
  which output(s) receive the stream; the adapter maps
  `{ display_name, host, port }` onto the daemon's device list at
  open time and re-checks it, so a receiver that vanished or renamed
  fails loudly instead of playing to the wrong device. The enabled
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
  guidance.
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
  re-applied — and a crashed holder releases it by OS semantics;
  the §4.4 revalidation remains the backstop for external clients
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
  own task — GStreamer adapters sample pipeline state there, the
  daemon adapter samples the JSON API player progress
  (`item_progress_ms`/`item_length_ms`). Paused sessions freeze,
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
  duration through the §4.1 observation cache, sampled from the JSON
  API player progress (`item_progress_ms`/`item_length_ms`); the
  500 ms publication cadence is unchanged (§4.4), and paused and
  disconnected behavior follow the seam's contract. Receiver latency
  is invisible to the UI, as today — documented, not hidden.
- **Multi-room:** out of scope unless separately approved (task.md
  P2.4); the adapter enables exactly the one discovered device the
  user activated — which, given the server-wide enabled-output set
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
  reachable, API version compatible, pipe creatable. Each failure
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
   `AirplaySender` returning `Err` makes `finish_load` emit the
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
   `src/audio/airplay_output.rs:293-305`), leaves no receiver-side
   playback continuing, and returns `Stopped` for the exact load
   generation; on the daemon adapter it also restores the state
   recorded at takeover — player stopped, our queue items removed,
   the recorded enabled-output set re-applied (§4.3).
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

**Platform scope:** items 1-8 run on the package targets the §8
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
3. **Discovery/AirPlay-2 enablement (separate, only after 2
   validates).** Extend `DiscoveredServer` for whatever the daemon
   mapping needs (§3, consequence 2), flip the §3 discovery filter,
   add AP2 real-device coverage.
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
  ([`docs/task.md:31-33`](task.md) — **16/57** implementation
  records complete, retained baseline **16/39**, kept synchronized
  with that file's literal counters) stays ahead of this work in the
  backlog order.
- Its only changes outside its own file are the two cross-references
  this branch already carries — the P2.4-C in-flight note in
  [`docs/task.md`](task.md) (checkbox intentionally unchanged; the
  item closes on an accepted design, not on this record) and the
  follow-on note in
  [`docs/release-component-policy.md`](release-component-policy.md).
  The substantive updates to both — the dependency decision, the
  shared-policy containment run, the changelog entry — belong to the
  implementation records that accept this design.
