# AirPlay sender design investigation

Status: design record, no implementation in this bead.

Closes the P2.4 "Open and complete an AirPlay sender design investigation"
checklist item ([`docs/task.md:1009-1011`](task.md)) and is the first
record on the maintained AirPlay sender path that the P2.4 work stream
must select. The investigation exists because the currently shipped seam
(`uridecodebin ! audioconvert ! avenc_alac ! raopsink`) is gated on a
GStreamer `raopsink` element that **no current official GStreamer,
Homebrew, or MSYS2 package ships** — see the 2026-07-20 sender review
recorded in [`docs/release-component-policy.md:87-96`](release-component-policy.md)
and the module-level comment in
[`src/audio/airplay_output.rs:30-41`](../src/audio/airplay_output.rs).
The P2.9 remediation removed the broken `shairport-sync`-as-sender fallback
("[tracker item P2.9](task-remediation-2026-07.md)") and now reports
AirPlay 1 unavailable rather than silently spawning a subprocess that
could never reach the selected receiver.

This document is the *result of the investigation*: an architectural
seam description, a survey of the candidates that could fill it,
provenance and licensing treatment for each, packaging consequences, the
test contract the seam must satisfy, and a concrete next-record plan.
It deliberately does not implement any one candidate. Choosing and
shipping a sender is a separate bead — the planning follows once a
maintainer accepts one of the proposals below.

## 1. Problem statement

The current seam:

```
uridecodebin ! audioconvert ! avenc_alac ! raopsink
                                              ^^^^^^^^
                       enforced by AirPlayOutput::ensure_raopsink
                       (src/audio/airplay_output.rs:266-285),
                       wired through open_session / open_resolved_session /
                       open_local_session / open_prepared_media
                       (src/audio/airplay_output.rs:159-205),
                       built by build_raop_pipeline
                       (src/audio/airplay_output.rs:434-460).
```

Every AirPlay 1 load gates on `gst::Registry::get().find_feature("raopsink", ...)`.
When the element is absent (the documented current state on every
supported PACKAGED deployment, absent a separately supplied compatible
raopsink), `ensure_raopsink` returns the localized
`errors.playback.airplay_raopsink_missing` error and the pipeline is
never built. Tests `a_missing_raopsink_is_refused_with_honest_guidance`,
`raopsink_guidance_is_localized_for_every_catalog`, and
`a_missing_raopsink_load_fails_loudly_not_silently`
([`src/audio/airplay_output.rs:721-781`](../src/audio/airplay_output.rs))
pin that contract.

The seam itself is fine — it is the *element* that does not exist. To
unblock real AirPlay 1 sending we need either (a) a maintained, packaged
GStreamer `raopsink` element, or (b) a different transmission path that
replaces the `raopsink !` tail with another mechanism. AirPlay 2 is a
significantly larger lift (mDNS-SD pair-setup with X25519 + ed25519,
ChaCha20-Poly1305 RTSP, optional FairPlay audio), and is **out of scope
for this investigation's near-term sender decision.** The sender-design
record commits to selecting and validating a maintained sender path;
that selection is recorded here.

## 2. Seam design

The seam already has the right structural shape: a registry-gated gate,
a load-time placement of `ensure_raopsink` *before* any per-track proxy
work (see `protected_load_fails_closed_before_any_pipeline_sees_the_secret`
at [`src/audio/airplay_output.rs:782-839`](../src/audio/airplay_output.rs)),
and a per-session pipeline tied to the generation-scoped event channel.
To make it swappable between raopsink-like elements and the
non-GStreamer alternatives surveyed below, the seam should be split into
a small trait plus one adapter per implementation. This is the smallest
change consistent with the investigation's expected near-term outcomes
and is proposed here as the *design contract*, not as code to land in
this bead.

### 2.1 Proposed `RaopSender` trait

```rust
/// A pluggable RAOP transmitter that owns the tail of the audio
/// pipeline (post ALAC encode) and routes the encrypted RTP stream to
/// the selected receiver. Implementations are responsible for the
/// protocol negotiation (RTSP OPTIONS / ANNOUNCE / SETUP / RECORD)
/// and the receiver-key publication look-up, not the audio-side
/// decoder or encoder — those stay in the existing
/// `uridecodebin ! audioconvert ! avenc_alac` head.
trait RaopSender: Send + Sync {
    fn name(&self) -> &'static str;
    /// Returns `Ok(())` when this sender can be constructed on the
    /// current host. Returning `Err(reason)` surfaces the existing
    /// localized AirPlay-unavailable guidance so the load path can
    /// reuse the gating from `ensure_raopsink`.
    fn probe(&self) -> Result<(), String>;
    /// Build the sink tail for a receiver identified by `host:port`,
    /// plugging onto an existing ALAC-encoded audio pad.
    fn build_sink_tail(
        &self,
        host: &str,
        port: u16,
        volume_db: f64,
    ) -> Result<gst::Element, String>;
}
```

Existing `AirPlayOutput::ensure_raopsink` /
`AirPlayOutput::build_raop_pipeline` become one implementation
(`GstRaopsinkSender`). Each non-GStreamer candidate in §3 supplies
a new adapter. Load paths pick an implementation at
`open_prepared_session` time via `gc.config` / a future capability
flag — never silently, never per-track.

### 2.2 What must NOT change in this refactor

- The fail-closed ordering: `ensure_raopsink` (or its replacement
  `probe`) runs *before* the app-owned exact-route proxy mints a
  loopback ticket for the URI. The existing
  `protected_load_fails_closed_before_any_pipeline_sees_the_secret`
  test must still pass without modification.
- Position/duration evidence continues to flow on the same 500 ms
  generation-scoped timer. AirPlay 1 receivers buffer the audio
  before emission and report nominal timing that lags the sender clock
  by the receiver's own latency; the existing position-sampling code
  ([`src/audio/airplay_output.rs:417-430`](../src/audio/airplay_output.rs))
  samples from the pipeline state, not from the receiver, so any
  sender adapter must expose a pipeline whose state transitions remain
  observable to the bus watch.
- Localization. The honest "AirPlay 1 unavailable" message stays
  user-visible in every catalog (`locales/*.yml`,
  `errors.playback.airplay_raopsink_missing`) regardless of which
  sender adapter is selected. Renaming the message to a generic
  "AirPlay sender unavailable" is acceptable *only* once a candidate
  sender actually ships; until then, calling the missing dependency
  `raopsink` is what the test contract pins.
- The registry-gated contract remains: the absent-element branch is
  never re-routed to an unrelated subprocess (the P2.9 lesson).
  Every adapter's `probe` returns its own structured error; the load
  path surfaces it directly without falling back to spawning or to a
  legacy "shairport-sync" path. No new `try_alt / or` macro, no new
  branch of `open_prepared_session` that pipes PCM out-of-process.

## 3. Sender candidates

This survey is the *maintained* sender universe as of the 2026-07-20
review. Each entry states what it is, whether it is a sender or a
receiver, its distribution / packaging status, and its consequences for
[`build-aux/packaging/forbidden-bundled-components.txt`](../build-aux/packaging/forbidden-bundled-components.txt)
and the release-component review boundary.

### 3.1 GStreamer `raopsink` — *historical/unmaintained*

- **What it is / was.** The GStreamer gst-plugins-bad AirPlay 1 sink.
- **Status.** Removed from gst-plugins-bad upstream after remaining
  unported (commit
  `9b5de053995488d5ddc78c1bf4df651101271d70`). Its historical
  differently-named `apexsink` legacy implementation embedded only an
  RSA public modulus/exponent used to encrypt a generated outbound
  session key (not a private key or DRM-protected-media decryptor);
  that distinction is provenance evidence, not a maintained
  sender path. No current official GStreamer, Homebrew, or MSYS2
  package ships the element.
- **Verdict.** Non-option. This is the dependency that triggered the
  P2.9 removal and that this investigation is selecting an
  alternative for. No realistic path to ship it without first
  reimplementing the protocol (see §3.4).

### 3.2 `shairport-sync` — *AirPlay 1 receiver, not a sender*

- **What it is.** A maintained Linux daemon that receives AirPlay 1
  audio. Distributed in Homebrew and most Linux distributions as a
  service binary.
- **Why it is not a sender.** `shairport-sync` advertises itself as
  an AirPlay 1 receiver; piping PCM into it (the pre-P2.9 fallback)
  ignores the device the user selected and cannot transmit to it
  ([review finding M3, `docs/task-remediation-2026-07.md:1351-1386`](task-remediation-2026-07.md)).
  `shairport-sync` does not expose a "I am now a sender" mode.
- **Verdict.** Remain a *gateway for any Tributary host that wants
  to publish audio from a different device to its own speakers*,
  not a path for Tributary to publish to user-selected receivers.
  Out of scope for the sender selection.

### 3.3 `libshairplay` / `libraop` and forks — *receiver libraries*

- **What they are.** C libraries reverse-engineered from observing
  AirPlay 1 traffic. Distributed by some Linux distributions
  (`libshairplay-dev` on Debian-derived, `libshairplay` elsewhere).
- **Status.** These are *receiver* libraries. They implement the
  RTSP/AES session side from the *receiving* end (decrypting RTP
  payloads). Adapting them as senders would require rewriting
  the encryption half and is not a maintained sender path.
- **Verdict.** Non-option for the sender decision. Their protocol
  knowledge is, however, the basis of every documented AirPlay 1
  sender implementation to date, including the legacy `apexsink`
  (§3.1).

### 3.4 Hand-rolled RAOP sender (Tributary-owned)

- **What it is.** A small, focused RAOP client written in Rust that
  performs the RTSP OPTIONS / ANNOUNCE / SETUP / RECORD handshake,
  wraps ALAC frames in RTP/UDP, and AES-CBC-encrypts each packet
  with the session key derived from RSA-OAEP-encrypted server
  public key. Ships as a `gst_element_factory_make("tributaryraopsink")`
  inside a new `gst-plugin-tributary` crate, or as a standalone
  process invoked through the upstream proxy boundary at the pad
  tail — the latter is preferable for first review because it keeps
  the GStreamer-touching diff minimal and matches the
  generation-scoped event channel's threading model.
- **Status.** Not in tree. No published external maintained
  implementation of RAOP-as-sender exists that is both packaged
  in a major distribution *and* of a quality level acceptable for
  the [`docs/release-component-policy.md`](release-component-policy.md)
  review boundary.
- **Provenance / licensing of the embedded key.** RAOP receivers
  publish an RSA public key (`pk`) in their mDNS TXT record. The
  sender encrypts a fresh random AES key with that key. Only the
  **public** half is needed. The release-component policy reminds us
  that "a key being public rather than private does not establish
  that its provenance or distribution is appropriate" — see
  [`docs/release-component-policy.md:80-86`](release-component-policy.md).
  Implementations that bundle a *fixed* receiver key database (or a
  hash table of canonical receivers) are in scope of the review
  boundary. Implementations that fetch the public key from the
  receiver's mDNS record at runtime (the only correct RAOP-1 sender
  behavior) carry no embedded key material and need only document
  the protocol compliance of that fetch.
- **DRM-bypass review boundary.** RAOP-1 protects only the on-air
  audio for streaming. There is no DRM, no copy-control
  circumvention, no decryption-of-encrypted-content on the client
  side. The release-component policy's *review-boundary* clause
  still applies ("a sender implementation that embeds protocol key
  material... requires a dedicated design and distribution review.
  Update the shared policy, this document, tests, and changelog
  together, with the reason and artifact evidence recorded in
  review" — [`docs/release-component-policy.md:79-86`](release-component-policy.md)).
  A subsequent bead that lands this candidate must record both the
  rationale and the artifact evidence in this document and call for
  an update to `docs/release-component-policy.md` alongside the
  implementation PR.

### 3.5 AirPlay 2 sender — *deferred*

- **What it is.** Public AirPlay 2 protocol knowledge is collected
  in community projects; the most-cited reference is the
  `airplaysdk` and `airplay2-receiver` projects. Implementations
  remain receiver-focused. A sender requires mDNS-SD pair-setup
  with X25519 + ed25519, ChaCha20-Poly1305 RTSP control, and
  Apple-Lossless over RTP-AAP. FairPlay audio is not required for
  user-supplied audio but is required by some copyrighted streams;
  Tributary does not engage with FairPlay streams.
- **Status.** No maintained open-source sender is shipping. A
  from-scratch sender is multi-month work and would also call the
  release-component review boundary when it embeds protocol key
  material.
- **Verdict.** Out of scope for the P2.4 sender-selection record.
  AirPlay 2 receivers continue to be filtered from the output
  selector ([`src/audio/airplay_output.rs:30-36`](../src/audio/airplay_output.rs),
  [`src/ui/discovery_handler.rs:45-63`](../src/ui/discovery_handler.rs)).

### 3.6 Recommendation

**§3.4 is the only path that does not regress to the broken `shairport-sync`-as-sender fallback.** It is also the path that requires the most care:
the implementation must land alongside a release-component review
record in `docs/release-component-policy.md` (update the policy
section, the helper-policy list, the changelog, and add focused tests
together — that's the review contract), a packaging bundle audit (the
new sender library must not pull a denied filename token, regardless
of direct/indirect linkage), and a real-device test plan (§5).

The seam refactor in §2 is the precondition for §3.4: once the
`RaopSender` trait exists, the implementation PR can land an
`AudioOutputProxy::new(... "raop_proxy")` adapter that forks an
embedded sender child-process or loads a Rust `raop` crate module
inline, and existing tests continue to pin the load path's
fail-closed ordering.

## 4. Pairing, encrypted control, audio, timing

The protocol sequence a sender must implement is the documented RAOP-1
control flow; this section summarises what the implementation PR must
nail down so that future readers do not have to reconstruct the wire
layout from upstream source.

- **Service discovery.** Receivers advertise `_raop._tcp.local.`
  with a TXT record carrying at minimum `pw` (the RSA public modulus,
  base64) and `et` (encryption type, e.g. `0,1` = RSA-OAEP,
  AES-CBC/IV). Tributary's mDNS browse of `_raop._tcp.local.` already
  produces `service_type = "airplay"` entries
  ([`src/discovery.rs:343-379`](../src/discovery.rs)); downstream
  `DiscoveredServer` rows already carry the TXT record in `txt`.
- **Pairing.** First connection requires RSA-OAEP encryption of an
  ephemeral AES-128 session key with the receiver's published
  modulus. The receiver decrypts with its private half and replies
  with the AES IV. Subsequent connections cache the session for the
  receiver's lifetime if PIN is not required, or are rejected when
  pairing is enforced. Tributary must surface a localized "pairing
  rejected / failed" message in addition to the existing
  `airplay_raopsink_missing` message when the selected adapter fails
  at this stage.
- **Encrypted control.** RTSP / ANNOUNCE / SETUP / RECORD are sent
  over a TLS-PSK channel derived from the AES session key, plus the
  optional timing / event / cover-art streams over additional UDP
  ports. Only the audio RTP/UDP stream and the timing UDP stream
  are required for functional playback. Metadata and cover art are
  out of scope for the first sender record.
- **Audio.** 16-bit signed little-endian PCM, 44.1 kHz, two channels,
  ALAC-encoded in 4096-frame packet sizes. `avenc_alac` already
  ships in `gst-libav`/`ffmpeg` and is the existing encoder
  ([`src/audio/airplay_output.rs:441`](../src/audio/airplay_output.rs));
  no encoder change is needed for any §3 candidate.
- **Timing.** Tributary's existing position/duration sampling
  ([`src/audio/airplay_output.rs:417-430`](../src/audio/airplay_output.rs))
  is sender-clock based; receivers do not feed back a wall-clock
  position. The 500 ms polling interval is documented as the
  playback-history contract
  ([`docs/playback-history.md:88-89`](playback-history.md)) and
  must stay unchanged across sender adapters.

## 5. Packaging consequence

A new RAOP sender library, whether hand-rolled or vendored, lands
inside a bundled binary on macOS/Windows releases and as a system
package on Linux/Flatpak. The packaging audit must:

- Confirm no filename token in
  [`build-aux/packaging/forbidden-bundled-components.txt`](../build-aux/packaging/forbidden-bundled-components.txt)
  appears in the bundled payload, transitively. The current list
  focuses on DVD/Blu-ray/DRM decryptors and is unaffected by RAOP-1;
  the audit is a *positive confirmation* that the new library
  introduces no denied component.
- Re-run the full shared-policy containment pipeline from
  PR #152 — Windows ZIP/PE gate, macOS app+installer tree gate,
  native Linux package gate, Flatpak app-commit gate. The non-Linux
  gates require a built binary; the implementation PR must record
  the artifact-level evidence in the bead notes, not just the
  filename-token scan.
- Update `CHANGELOG.md` with the new sender and link to the policy
  review entry in this document.

## 6. Real-device tests

The current test suite
([`src/audio/airplay_output.rs:721-839`](../src/audio/airplay_output.rs))
covers the *absence* path with high specificity:

- `a_missing_raopsink_is_refused_with_honest_guidance`
- `raopsink_guidance_is_localized_for_every_catalog`
- `a_missing_raopsink_load_fails_loudly_not_silently`
- `protected_load_fails_closed_before_any_pipeline_sees_the_secret`

The implementation PR must add:

- A `RaopSender::probe` regression that pins: when an adapter's
  registry/library path is missing, the load is refused *before*
  any per-track proxy work, regardless of which adapter was
  selected by config. Mirrors today's `ensure_raopsink` contract.
- An adapter-injection unit test that proves a stub `RaopSender`
  returning `Err("...")` causes `finish_load` to emit
  `PlayerEvent::Error` with the supplied message, generation-tagged,
  followed by `PlayerEvent::StateChanged { Stopped }` — identical
  shape to today's `a_missing_raopsink_load_fails_loudly_not_silently`.
- A registry-attribute regression: the existing `find_feature` look-up
  must remain the source of truth for the GStreamer-backed adapter,
  and a GStreamer-pretending adapter (a fake `ElementFactory::upcast_ref`)
  must still trip `probe` if it does not match the expected factory
  class.
- A real-device integration test, **gated behind a hardware receiver
  presence check** (`AIRPLAY_TEST_RECEIVER` env var), that drives
  one TRACK through the chosen adapter against a reachable RAOP-1
  receiver on the tester's network. This test must not run in CI
  by default; the gate variable signals opt-in. Skipping the test
  does not flip CI green-to-red.

## 7. Proposed next-record plan

The investigation does not itself implement any candidate. The
selection-and-implementation track is proposed as one parent bead
with three child beads:

1. **Land the `RaopSender` trait + the existing `GstRaopsinkSender`
   adapter.** Mechanical refactor; no behavior change; the
   existing tests continue to pass. Locked `cargo check`,
   `cargo clippy` (debug + release), `cargo test --all-targets`.
2. **Release-component policy update.** Record the rationale in
   [`docs/release-component-policy.md`](release-component-policy.md)
   ("Review boundary" section), update the changelog, run the full
   PR #152 containment pipeline against a build that contains the
   new library. No audio-side behavior change.
3. **Implement the §3.4 sender.** Add the adapter; gate-keep with
   `RaopSender::probe`; expand the test plan from §6. Locked test
   suite plus a real-device integration test gated on
   `AIRPLAY_TEST_RECEIVER`. Update the receiver-side `DiscoveredServer`
   contract if the sender needs additional TXT fields beyond the
   existing `pw`/`et` handling.

## 8. What this investigation deliberately does not do

- It does not ship an embedded RAOP-1 sender library.
- It does not loosen the `forbidden-bundled-components.txt` denial.
- It does not propose a new packaging source until the release
  review records both the rationale and the artifact evidence.
- It does not enable AirPlay 2 sending. AirPlay 2 receivers remain
  filtered from the output selector by
  [`src/ui/discovery_handler.rs:45-63`](../src/ui/discovery_handler.rs).
- It does not promise an exact target date. The 14/38 P2.1 feature
  focus ([`docs/task.md:152-167`](task.md)) remains ahead of
  this work in the backlog order.
