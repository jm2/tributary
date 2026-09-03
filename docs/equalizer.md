# Equalizer contract

- Status: design only — P2.4 equalizer design record; implementation pending
- Decision date: 2026-07-24
- Tracking issue: [#49](https://github.com/jm2/tributary/issues/49)
- Backlog entry: [`task.md`](../task.md) (P2.4)
- Roadmap rationale: [`roadmap.md`](../roadmap.md) (Audio-output plan, item 1)

This document defines the equalizer feature for Tributary: what the user can adjust, how each output
backend is classified, where DSP runs in the pipeline, how live reconfiguration is bounded, what is
persisted, and which behaviors are explicitly unsupported. The feature is intentionally small and
parity-bounded with the rhythmbox-era equalizer that prompted issue #49. Multi-band parametric
equalization, room correction, and per-track profiles are out of scope.

## Scope

Tributary offers a single ten-band parametric-style equalizer with five named presets, a
write-only `Custom` state for manually edited bands, a global preamp, and a soft-knee signal
compressor that engages above −6 dBFS with an asymptotic output ceiling at 0 dBFS to reduce the
likelihood of hard clipping. The equalizer runs in the **local** pipeline only. AirPlay (RAOP),
Chromecast, and MPD outputs are explicitly listed with their supported, partially supported, or
unsupported status and the user-visible behavior for each non-supported state. The settings UI
surfaces the equalizer unconditionally; for unsupported outputs the controls are rendered
disabled with a closed-form explanation.

Future work (multi-band parametric, room correction, per-track profiles, per-source overrides,
lossy-format adaptive EQ, LUFS/R128 loudness normalization) requires a refined implementation
record and is not addressed by this contract.

## Bounded user surface

The equalizer exposes exactly the following controls. Each control has one bounded type, a fixed
range, a fixed precision, and a fixed default. Settings outside these bounds are invalid input and
the settings UI must reject them at the boundary; runtime code must not be able to materialize
them.

| Control | Type | Range / values | Precision | Default |
|---|---|---|---|---|
| Enabled | bool | `true` / `false` (global bypass) | — | `false` |
| Preset | enum | `Flat`/`Pop`/`Rock`/`Jazz`/`Classical`/`Custom` (write-only) | — | `Flat` |
| Preamp | linear dB | `−24.0` … `0.0` … `+12.0` dB; integer or half-step | 0.5 dB | `0.0` dB |
| Bands 1..10 | linear dB | `−24.0` … `0.0` … `+12.0` dB | 0.5 dB | `0.0` dB |
| Clip protection | enum | `Off` / `Soft` (see the clip-protection paragraph below) | — | `Off` |

Ten band center frequencies are taken from the canonical `equalizer-10bands` element
(`gst-plugins-good` 1.28.5, verified with `gst-inspect-1.0`): **29 Hz, 59 Hz, 119 Hz, 237 Hz,
474 Hz, 947 Hz, 1889 Hz, 3770 Hz, 7523 Hz, 15011 Hz.** These centres are fixed by the upstream
GStreamer element; the per-band integer-Q (1.0 octave) cascade is internal to the element. Custom
centres are reachable through the element's `GstChildProxy` interface as `band0::freq`…
`band9::freq` (one child element per band; `freq` is in Hz), but adopting custom centres is a
contract change and requires an updated acceptance matrix. This contract freezes the canonical
ten centres listed above.

Each named preset is a fixed vector of ten band gains in dB and a recommended preamp. Presets
are immutable; loading a preset always replaces the entire band vector *and* the preamp to its
recommended value. Once the user manually edits any band or the preamp, the persisted preset
name transitions to `Custom` (see *Band and preamp mechanics* below); the named preset is no
longer the source of truth and is over-written by `Custom` in storage.

| Preset | Recommended preamp | Description |
|---|---|---|
| Flat | 0.0 dB | All bands at 0.0 dB (the default state and the empty baseline) |
| Pop | −2.0 dB | Bass shelf + slight mid cut + presence lift |
| Rock | −1.0 dB | Bass + treble lift, mid dip |
| Jazz | −1.0 dB | Bass + mid lift |
| Classical | −2.0 dB | Slight treble lift, flat low end |

Pop/Rock/Jazz/Classical gain vectors are documented in the appendix of this contract and are not
free-form; reviewers must reject any PR that introduces a new band value without the matching
entry in the appendix.

`Clip protection` is a global safety feature independent of the equalizer engine. When `Soft` is
enabled, the engine inserts an `rglimiter` element immediately after the equalizer stage. The
element's own description in GStreamer is "Apply signal compression to raw audio data" — it is a
**soft-knee signal compressor**, not a peak limiter. With `enabled=true` it applies a fixed
−6 dBFS threshold above which it compresses progressively, with an asymptotic output ceiling of
0 dBFS: it never lets the signal exceed full scale, but the asymptotic nature means levels near
the ceiling are heavily compressed rather than held flat. The element exposes no user-tunable
parameters — attack, release, threshold, and ceiling are all fixed by the element. The protection
is therefore "the compressor prevents the output from exceeding full scale," not "the compressor
prevents inter-sample peaks": a 0 dBFS ceiling provides no inter-sample headroom, and a
soft-knee compressor engaging from −6 dBFS will compress most of a typical modern programme
rather than catching only occasional peaks. When `Off`, no limiter exists in the pipeline and
clipping can occur; the state is preserved for power users who explicitly want raw output.

`Enabled = false` is global bypass: the equalizer filter (and its preamp stage) is *not inserted*
in the local-output pipeline at all, so the pipeline reduces to the existing passthrough chain
without disturbing the persisted settings. Toggling `Enabled` back to `true` re-inserts the
equalizer stage with the persisted bands and preamp intact. The default for fresh installs is
`false`, and a fresh install therefore does not touch the existing pipeline shape at all.

## Filter graph

All equalizer DSP runs in the local-output `playbin3` pipeline by installing a single
`playbin3`-managed audio-filter bin between the decoder output and the audio sink. The bin owns
one src pad (`audio-filter-src`) and one sink pad (`audio-filter-sink`); `playbin3` links the bin
to its audio stream exactly once during the pause/relink seam and unwinds the link when the bin
is removed. None of the elements inside the bin is substituted for a different one without a
contract change.

The bin carries a `caps` property pinned to
`audio/x-raw, format=F32LE, channels=2, layout=interleaved, rate=<samplerate>` (where
`<samplerate>` is the rate `playbin3` negotiated with the decoder on the bin's sink pad at
chain-construction time). `playbin3` uses this caps property to negotiate the upstream format;
if the upstream decoder cannot deliver that caps — typically only on a malformed or non-PCM
source — the bin's sink pad `activate` mode returns `FALSE`, `playbin3` propagates the error to
the bus, the implementation does **not** insert the bin, and the pipeline falls back to the
existing passthrough layout (a single info-level diagnostic names the source URI). This is the
spec's only rollback path: there is no element-by-element fallback inside the bin, and a failed
`audio-filter-caps` negotiation does not leave the chain half-inserted.

The chain layout for the *enabled, clip-protection-on* state is:

```text
playbin3.audio-filter (bin "eq-bin") {
    audioresample !
    audioconvert !
    capsfilter caps="audio/x-raw,format=F32LE,channels=2,layout=interleaved" !
    volume       name=eq-preamp    volume=<factor> !
    equalizer-10bands name=eq
        band0=<gain> band1=<gain> ... band9=<gain> !
    rglimiter   name=clipper       enabled=true !
    audioconvert !
    audioresample !
    capsfilter  caps=<sink-caps>
} !
playbin3.audio-sink
```

Where:

- `audioresample` (`gst-plugins-base`) is the sample-rate converter. It resamples the upstream
  decoder's negotiated rate to the rate the bin's caps property pins (above). Two instances live
  in the chain: one before the EQ stage (to set the rate the biquad and limiter operate on) and
  one after (to bring the rate back in line with the audio sink's negotiated rate on the way out
  in case the limiter has shifted it).
- `audioconvert` (`gst-plugins-base`) is the format/channel converter. Two instances flank the
  EQ stage: one before the EQ (to convert to the format/channel layout the biquad and limiter
  expect) and one after (to convert back to whatever the sink accepts). `audioconvert` does *not*
  change sample rate, so it cannot satisfy the limiter's F32LE requirement on its own; the
  `capsfilter` does the format pinning.
- `capsfilter` (`gst-plugins-base`) pins audio caps on a pad. The pre-EQ `capsfilter` pins the
  format the biquad and limiter both accept (`audio/x-raw, format=F32LE, channels=2,
  layout=interleaved`). The post-EQ `capsfilter` pins to `<sink-caps>`, the audio sink's
  negotiated caps filled in by `playbin3` at chain-construction time. This is the only element
  in the chain that *negotiates* a format; `audioconvert` does conversion work, `capsfilter`
  enforces the boundary.
- `volume` (`gst-plugins-base`, plugin `volume`) provides the preamp. `volume` is a multiplicative
  gain element whose `volume` property ranges `0.0` to `10.0`, with `1.0` meaning unity (0 dB).
  The preamp dB value is converted to a factor at write time as `factor = 10^(dB/20)`, so the
  −24.0…+12.0 dB range maps to factor `0.0631`…`3.9811`, all well inside the element's `0..10`
  range. A preamp of `0.0 dB` writes `volume=1.0` and the element passes audio unchanged.
  Preamp is one of the five deliverables named by the bead; the contract pins it to this element
  rather than to a property of `equalizer-10bands`, which has no preamp gain stage of its own.
- `equalizer-10bands` is the canonical `gst-plugins-good` element. It exposes ten fixed band
  gain properties, `band0`…`band9`, in dB; the element implements `GstIirEqualizer` (a fixed-Q
  parametric) and `GstChildProxy`, so per-band centre frequency is reachable as `band0::freq`…
  `band9::freq` if a future contract revision adopts custom centres. This contract uses the
  element's canonical centres and does not touch `bandN::freq`; `band0` (29 Hz) through `band9`
  (15011 Hz) are written as flat `bandN=<gain>` properties.
- `rglimiter` (`gst-plugins-good`, plugin `replaygain`) is the clip-protection limiter. Its only
  behavioural property is `enabled` (Boolean, default `true`); the element's own description is
  "Apply signal compression to raw audio data". When `enabled=true` it acts as a soft-knee
  compressor with a fixed −6 dBFS threshold, smoothly compressing levels above the threshold with
  an asymptotic output ceiling at 0 dBFS. There is no brick-wall behaviour and no inter-sample
  headroom: the element prevents output from exceeding full scale, but a 0 dBFS signal at the
  limiter's input is compressed by approximately 1.1 dB before it reaches the sink. The element
  exposes no user-tunable parameters — attack, release, threshold, and ceiling are fixed by the
  element. Clip protection is one of the five deliverables named by the bead; the contract pins
  it to `rglimiter` rather than to `audioamplify`, which is not a limiter (it is a static
  amplifier with hard-clip / wrap / none options, no envelope follower, and no `max-amplitude`
  property).

`equalizer-10bands` runs at 32-bit floating-point internally. The pre-EQ capsfilter pins
`audio/x-raw, format=F32LE, channels=2, layout=interleaved` so the biquad and the limiter's F32LE
requirement are both satisfied by the format the bin negotiates upstream of `equalizer-10bands`.
The post-EQ `capsfilter` (with `<sink-caps>` filled in at chain-construction time) re-pins to the
audio sink's negotiated caps, which may be a different rate, channel count, or signedness but is
not constrained to F32LE. `playbin3`'s gapless navigation keeps the same bin installed across URI
transitions; the equalizer state is not re-applied automatically on each new URI (see
*Live-reconfiguration boundary* below).

The chain layout for the *enabled, clip-protection-off* state is the bin without the
`rglimiter` element. The `rglimiter`'s `enabled` flag is moot in this state because the element
is not present in the bin.

The chain layout for the *disabled* state is the existing pipeline without the equalizer bin at
all. `playbin3.audio-filter` is `NULL`. The persisted bands, preamp, and clip-protection
setting remain on disk but the bin is not installed, so toggling `Enabled` back to `true`
requires the pause/relink seam (see *Live-reconfiguration boundary* below) to install the bin
fresh.

The equalizer never reaches the receiver-side path; AirPlay, Chromecast, and MPD outputs do not
have an equalizer chain and never will under this contract.

## Band and preamp mechanics

Each band implements a peak-filter biquad whose center frequency, Q, and gain are properties of
the element. The element's gain is set in dB and applied to the biquad coefficients in real time;
the filter is therefore a *fixed-Q* parametric, not an arbitrary IIR. The ten canonical band
centres (29 Hz through 15011 Hz) are the element's own; bands are written as `band0` (29 Hz)
through `band9` (15011 Hz).

The preamp is a single linear gain applied by the dedicated `volume` element *before* the band
stack. Selecting a named preset (`Flat`, `Pop`, `Rock`, `Jazz`, `Classical`) writes that
preset's full band vector and recommended preamp, and sets the persisted `preset` field to the
preset's key. After this write, manual edits to any band gain or to the preamp move the
persisted `preset` field to a sixth value, `Custom`; the UI displays `Custom` in the preset
combo to signal that the active vector no longer matches any named response. Loading a named
preset from the `Custom` state replaces the band vector and preamp with that preset's values
and sets the persisted `preset` field back to the named preset's key. `Custom` is *not*
selectable from the preset menu; it is a write-only field that the implementation sets as a
side-effect of a manual edit.

`Clip protection = Soft` inserts `rglimiter enabled=true` immediately after the EQ stage. The
element behaves as a soft-knee compressor with a −6 dBFS threshold and an asymptotic 0 dBFS
output ceiling. Limiter attack, release, threshold, and ceiling are fixed by the element; no
user-visible controls are exposed.

Pre-LP clip behavior is what clip protection actually guards against: the EQ can elevate peaks
above 0 dBFS even with a sane-looking preamp, especially on already-mastered pop/rock material.
When `Clip protection = Off`, the contract explicitly permits clipping and the application must
not pretend it was prevented. The `Soft` option is therefore the recommended default for fresh
installs starting with enabled EQ.

## Live-reconfiguration boundary

The contract intentionally limits which knobs can change mid-playback and at which pipeline stage
the changes take effect. The boundary is:

| Knob | Mid-playback? | Mechanism |
|---|---|---|
| `Enabled` | yes | Pause → bin install/remove → resume |
| `Preset` | yes | Buffer-boundary property-write transaction on the bin |
| `Preamp` | yes | Buffer-boundary property-write transaction on the bin |
| Single band `bandN` | yes | Buffer-boundary property-write transaction on the bin |
| Multiple bands at once | yes | Buffer-boundary property-write transaction on the bin |
| `Clip protection` | yes | Pause → `rglimiter` insert/remove inside the bin → resume |
| Band centres / Q | NO | Frozen by the spec; changing requires a new contract |

Live reconfiguration of `Enabled` installs or removes the equalizer bin at the
`playbin3.audio-filter` slot using the pause/relink seam:

1. Pause the pipeline (`gst::State::Paused`),
2. Set or clear `playbin3.audio-filter` as one edit (a single `GstBin` with the chain inside,
   or `NULL` when disabled),
3. Re-link the new audio filter (`audio-filter-sink` ↔ decoder output, `audio-filter-src` ↔
   audio sink), or remove the link when disabled,
4. Re-enter `Playing`,
5. Mark the change in metrics as a brief swap (≤ 100 ms by spec).

The persisted bands, preamp, and clip-protection setting are not touched on either side of the
transition.

Live reconfiguration of band gains, the preamp, and the preset is delivered through property
writes (`g_object_set`) on the running elements inside the bin so that coefficients update on
the next audio buffer without a pipeline state transition. No re-link, no re-instantiate, no
seek, no EOS-resending.

The *buffer-boundary transaction* the spec requires is a three-step sequence on the application
side:

1. Capture the new band vector and preamp into a single typed struct (`EqSettings`).
2. Wrap the property writes in `g_object_freeze_notify` / `g_object_thaw_notify` on each
   affected element (`equalizer-10bands` for the ten bands, `volume` for the preamp). Inside
   the freeze, each `g_object_set` only mutates the element's internal state; the
   `properties-changed` notification is suppressed until `thaw_notify` returns. The bus sees
   **one** `properties-changed` notification per element per transaction, not eleven.
3. Wait for the next `GST_MESSAGE_ELEMENT` carrying a `GST_EVENT_CAPS` or `GST_EVENT_SEGMENT`
   on the bus from `equalizer-10bands` or `volume`. That message marks the buffer boundary at
   which the new coefficients are picked up by the audio thread; `thaw_notify` returning
   *before* the buffer-boundary message is published does not mean the new coefficients have
   yet been read by the audio thread — it only means the property state is now visible to
   readers. Single-band writes skip the freeze/thaw wrapper and skip the boundary wait; only
   multi-property transactions (preamp changes, preset loads, multi-band batched edits)
   require it.

Because `g_object_set` is a GObject state mutation and produces no GStreamer pipeline event,
**no `Buffering` event is produced by configuration updates.** A `Buffering` event observed on
the bus during normal playback originates from the upstream decoder (network radio underrun,
file EOF, decoder flush) and is unrelated to the equalizer update; the UI must not show a
configuration-update spinner based on a `Buffering` event.

`Clip protection` toggles the `rglimiter` element's *presence* inside the bin (insert or
remove) using the same pause/relink seam applied at the element level (the surrounding bin
itself remains installed):

1. Pause the pipeline (`gst::State::Paused`),
2. Insert or remove the `rglimiter` element inside the bin (unlink, add, link),
3. Re-link the chain (`equalizer-10bands` ↔ `rglimiter` ↔ post-EQ `audioconvert`, or directly
   `equalizer-10bands` ↔ post-EQ `audioconvert` when the limiter is removed),
4. Re-enter `Playing`,
5. Mark the change in metrics as a brief swap (≤ 100 ms by spec).

A pause/resume swap is undesirable but is the only correct option for a topology change of
either kind. The contract accepts this cost because it is paid only on user-initiated
`Enabled` or `Clip protection` toggles, neither of which the user is expected to perform
frequently.

When the playing track is a live stream with no `gapless` table (e.g. a remote radio URL),
element insert/remove must not disturb the upstream decoder's buffering; the same
pause/insert/resume seam is used and a one-time metadata-free "reconfiguring audio output"
diagnostic is published. A failure to re-attach the limiter is treated as a recoverable error:
the chain degrades to the no-limiter layout and the user-visible status becomes the same as
`Clip protection = Off`.

## Persistence

Persisted equalizer state is six keys in a single file. The on-disk grammar is:

```ini
key="value"
key="value"
...
```

Where:

- Every key is one of the six listed below, in the listed order (the parser is order-insensitive
  on read but the writer always emits them in the canonical order so diffs and bugs are
  reproducible).
- Every value is a double-quoted UTF-8 string. Quotes inside a value are escaped as `\"`; the
  backslash is escaped as `\\`; newlines are not permitted in values. Floats are emitted with
  one decimal place (e.g. `"-24.0"`, `"0.0"`, `"+12.0"`) so that precision is bounded by the
  schema, not by the writer's locale or precision settings. The parser requires the value to be
  double-quoted; an unquoted value is a malformed file and triggers the validation rule below.

The six keys:

| Key | Type | Range / values |
|---|---|---|
| `schema_version` | quoted integer literal | `"1"` (the only supported value at this revision) |
| `enabled` | quoted boolean literal | `"true"` / `"false"` |
| `preset` | quoted preset name | `"flat"`/`"pop"`/`"rock"`/`"jazz"`/`"classical"`/`"custom"` |
| `preamp_db` | quoted float (one decimal) | `"-24.0"` … `"0.0"` … `"+12.0"` |
| `band0_db`…`band9_db` | quoted float (one decimal) | `"-24.0"` … `"0.0"` … `"+12.0"` |
| `clip_protect` | quoted enum | `"off"` / `"soft"` |

`equalizer.cfg` lives in the existing `dirs::data_dir()/tributary/` directory beside
`volume`. The file is owned by the equalizer module; no other module reads or writes it.

The writer uses an *atomic replace* protocol: it constructs the new content in memory, opens
`equalizer.cfg.tmp` next to the destination (mode `0600`, owned by the user), writes the entire
file in a single `write()`/`pwrite()` (the size is bounded by the schema — at most a few hundred
bytes even at the maximum band precision), `fsync`s the file descriptor, then `close()`s it,
then `rename(2)`s the temp file to `equalizer.cfg`, then `fsync`s the directory. After the
directory `fsync` returns, the new file is durable; an on-disk reader observes either the prior
file or the new file, never a partial one. The temp file is opened with `O_EXCL` so that a
concurrent writer cannot race the rename.

Persistence uses a debounced single-writer pattern: a 750 ms idle interval coalesces slider-drag
changes into one write per change-spell. The save runs on the GTK main loop and is suppressed
entirely when the state matches the fresh-install default (Enabled `false`, preset `Flat`, all
bands zero, preamp zero, clip protection `Off`). The debounce timer is reset on every change so
drag-induced writes are flushed on the trailing edge of the gesture.

In addition to the debounce, the equalizer module installs a *shutdown flush* hook: on
`gtk::main_quit` (and on `SIGTERM`/`SIGINT` via the application's main-loop signal hook), the
module synchronously performs an atomic-replace write of the current state to disk before the
GTK main loop exits. The shutdown flush is its own write-temp-and-rename cycle; it does not
wait for the debounce timer and runs even if the timer is armed. This guarantees that no
partial write is observable on disk if the user quits while the debounce is pending.

Fresh-install default state is exactly:

```ini
schema_version="1"
enabled="false"
preset="flat"
preamp_db="0.0"
band0_db="0.0"
…
band9_db="0.0"
clip_protect="off"
```

Migration from prior versions is out of scope; any pre-existing equalizer file from a different
schema is replaced with the default state on first load.

Validation rules on read:

- `schema_version` must equal `"1"`; any other value replaces the file with defaults.
- Each line must match the `key="value"` grammar; a malformed line replaces the file with
  defaults and the parser remembers which line failed for the diagnostic below.
- `bandN_db` values outside `[-24.0, +12.0]` are clamped to the boundary.
- `preamp_db` outside `[-24.0, +12.0]` is clamped to the boundary.
- `preset` outside the named set (including unknown legacy values) becomes `"flat"`; the band
  vector is *not* reset, only the persisted name is coerced.
- `enabled` not parseable as bool becomes `"false"`.
- `clip_protect` outside `"off"` / `"soft"` becomes `"off"`.

A malformed file is replaced with the default state via the same atomic-replace protocol above,
the user's prior preferences are recorded in a typed diagnostic with file path, byte count, and
the bad key, and the change is not silent.

## Capability matrix

The capability matrix below assigns a fixed status to each output for equalizer DSP. The status
is communicated to the UI through a new trait method, and the rendering is fixed per status; a
shipped output cannot claim an unsupported status, and a future implementation that adds a
status must ship the matching UI rendering at the same time.

| Output | Equalizer DSP |
|---|---|
| Local | supported |
| AirPlay (RAOP) | unsupported |
| Chromecast | unsupported |
| MPD | unsupported |

Reasoning, per output:

- Local: Pipeline owns the decoder-to-sink chain; the equalizer chain runs in process.
- AirPlay (RAOP): The receiving speaker renders audio; in-band equalizer protocol is
  proprietary and not exposed by the deployed receiver APIs.
- Chromecast: The receiving speaker renders audio; the Cast V2 protocol does not expose a public
  equalizer channel.
- MPD: MPD exposes server-side EQ commands (`eq`, `setvol`) that require server cooperation and
  vary by `libmpdclient` build; the canonical contracted behavior is host-side rendering, so
  host EQ does not reach the receiver.

For each `unsupported` output, the user-visible settings UI renders the equalizer controls as
disabled with a tool-tip explaining the limitation (e.g. "AirPlay receivers render audio
end-to-end, so Tributary's equalizer cannot reach the speaker.") Disabled controls preserve the
last-saved values locally.

The contract states one unambiguous output-activation rule: when the active output is
`unsupported`, the equalizer bin is *not* installed in any pipeline, no equalizer DSP runs, and
the local-output `playbin3.audio-filter` is `NULL`. The persisted bands, preamp, and
clip-protection setting remain on disk but are dormant — they are not lost, they are simply not
processed because no pipeline is rendering them. No CPU is spent on dormant equalizer DSP. When
the active output switches back to a `supported` output (e.g. `local`), the bin installs via
the pause/relink seam with the persisted configuration intact, so a later switch back to local
reflects the same persisted bands without further user action.

If a future output gains native equalizer support, the capability matrix moves that row from
`unsupported` to `supported`, gains a fixed contract for how the receiver side behaves
(e.g. MPD-side EQ command policy, receiver pre/post band alignment, preamp alignment), and
the UI rendering for that output unlocks the equalizer controls. No retroactive change is
applied to settings already stored on disk.

The settings UI therefore is the only place equalizer behavior visibly differs between outputs;
the audio pipelines carry the right state for their supported status, and the user does not
need to know that the equalizer module is renderer-specific.

## Preset and reset affordances

The settings UI exposes two affordances that affect the persisted state directly:

- **Reset to Flat.** Sets all bands to 0.0 dB, the preset to `Flat`, the preamp to 0.0 dB,
  keeps `Enabled` at its current value, and keeps `Clip protection` at its current value.
  Selecting `Flat` from the preset menu is a distinguishable UI action that also performs the
  same write but additionally flips the preset name to `Flat`.
- **Reload defaults from disk.** Forces the EQ module to re-read `equalizer.cfg`; this is the
  only way to remove a malformed file from disk. The debounced single-writer still applies
  on the next change-spell.

The UI must not offer "save preset", "rename preset", "delete preset", "export preset", or
"import preset" affordances. Naming a custom combination of band values as a new preset is
explicitly out of scope.

## Diagnostics

Diagnostics are bounded:

- A single informational log message is emitted when the equalizer chain is inserted into or
  removed from the local-output pipeline. The log carries boolean enabled state and the active
  preset name. It never carries individual band values, preamp values, or file paths.
- A single informational log message is emitted when the `rglimiter` is inserted into or
  removed from the local-output pipeline. It carries the boolean clip-protection state.
- The `equalizer.cfg` file path is logged at debug only, never at info or above.
- Diagnostic state on a malformed file is emitted at warn with the file path, byte count, and
  bad key only. The user's prior preferences are not dumped; the file content is not dumped.
- EQ metrics (e.g. peak amplitude per band, average gain) are deliberately not exposed in the
  UI or logs. Surfacing them would invite user-visible polish without a contract owner.

## Accessibility and localization

The settings UI shall advertise each equalizer control with its accessible label and the
keyboard accelerator that increases and decreases the value. Numeric bands must announce their
current value, the unit (decibels), and the boundary (e.g. `−6.0 dB, range minus twenty-four to
plus twelve`). The `Preset` combo box announces the active preset name and exposes only the
five named values listed above; `Custom` is shown in the combo but is *not* selectable from it
(only the five named values are clickable).

The settings UI is localized in the same locale set as the rest of the application (see the
files under `locales/`). The five named preset names are translated, but the keys stored in
`equalizer.cfg` remain English (`flat`, `pop`, `rock`, `jazz`, `classical`, `custom`). The
migration of older non-English keys is not expected; an unknown preset value is treated as
`flat`.

## Acceptance matrix

Implementation acceptance requires the exact conditions listed below. The matrix is exhaustive
for this contract; new conditions require a new revision.

1. **Fresh install, EQ disabled.** Persisted state matches the default; `playbin3.audio-filter`
   is `NULL`; no equalizer bin is constructed.
2. **Enable EQ on local output.** Equalizer bin is constructed and installed at
   `playbin3.audio-filter` via the pause/relink seam; `audio-filter-caps` is
   `audio/x-raw, format=F32LE, channels=2, layout=interleaved`; pipeline returns to `Playing`;
   total swap ≤ 100 ms.
3. **Change a single band mid-playback.** Single-property write reaches `equalizer-10bands`;
   buffer passes; no gapless discontinuity; new value reaches the filter on the next buffer.
4. **Select Pop preset mid-playback.** `EqSettings` struct captures ten bands + preamp; one
   `g_object_freeze_notify`/`thaw_notify` pair on `equalizer-10bands` writes all ten bands; one
   on `volume` writes the preamp; bus sees one `properties-changed` per element; preset combo
   displays `Pop`.
5. **Manual band edit mid-playback.** Single property write on `bandN`; persisted `preset` field
   becomes `custom`; UI combo displays `Custom`.
6. **Cycle clip protection Off → Soft → Off.** Pause/resume swap each time; `rglimiter` inserts
   inside the bin; total swap ≤ 100 ms per toggle.
7. **Sine input above +6 dBFS with clip protection = Soft.** Output peak converges
   asymptotically to 0 dBFS without exceeding it; soft-knee compression engages at the −6 dBFS
   threshold; a 0 dBFS input is attenuated by approximately 1.1 dB; reflects the `rglimiter`
   element's own description "Apply signal compression to raw audio data".
8. **Switch active output to AirPlay.** EQ module renders disabled in UI;
   `playbin3.audio-filter` becomes `NULL`; no equalizer DSP runs.
9. **Switch active output back to Local.** EQ module renders enabled in UI if previously
   enabled; bin re-installs at `playbin3.audio-filter` with persisted bands and preamp.
10. **Quit while a slider drag is in progress.** Shutdown flush hook writes the current state to
    disk via atomic replace before `gtk::main_quit` returns; no partial writes.
11. **Malformed `equalizer.cfg` on disk.** Defaults re-written via atomic replace; single
    warn-level diagnostic published (file path, byte count, bad key).
12. **Preamp outside bounds in saved file.** Value clamped to range; preset and bands remain
    valid.
13. **Band value outside bounds in saved file.** Value clamped to range; other bands remain
    valid.
14. **Preset name not in the named set on disk.** Coerced to `flat`; band vector remains as
    written on disk.
15. **Hardware sink with 8-channel layout (macOS).** Pre-EQ `audioconvert` caps remain `[1, 2]`;
    EQ runs in stereo; same cap fix as existing module.
16. **Gapless album transition.** Bin persists across URI transitions; equalizer state is not
    re-applied automatically on each new URI; no audible discontinuity.
17. **Preamp `0.0 dB` selected.** Preamp `volume` element is in the chain with `volume=1.0`;
    gain flat.

## Implementation boundary

The implementation record listed in `task.md` P2.4 line 1006 lands this contract. The record is
intentionally bounded: implement the supported path and the disabled-UI path, then validate the
acceptance matrix. Implementation is **not** covered by this design document; the design document
is the source of truth for the contract and changes only by revision.

The implementation record:

- Adds the equalizer bin (per the layout in *Filter graph*) to the local pipeline as
  `playbin3.audio-filter`, with the elements' spellings inside the bin
  (`audioresample ! audioconvert ! capsfilter
  caps="audio/x-raw,format=F32LE,channels=2,layout=interleaved" !
  volume name=eq-preamp volume=<factor> !
  equalizer-10bands name=eq band0=<gain> ... band9=<gain> !
  rglimiter name=clipper enabled=true !
  audioconvert ! audioresample ! capsfilter caps=<sink-caps>`) as specified in *Filter graph*
  above.
- Adds the `rglimiter` insert/remove inside the bin behind the single `Clip protection` boolean.
- Adds the `equalizer.cfg` reader and writer.
- Adds the trait method `AudioOutput::supports_equalizer` and implements it honestly per row
  of the capability matrix.
- Adds the settings UI panel with the bounded controls, preset combo, clip-protection combo,
  and the disabled-with-explanation rendering for unsupported outputs.
- Validates the acceptance matrix above; failures must be fixed, not silenced.

## Dated implementation boundary

Pending. No equalizer code, plugin wiring, settings UI, or persistence is implemented at the
date of this design. Future entries to this section will mirror the format used by
[`lastfm-scrobbling.md`](lastfm-scrobbling.md#dated-implementation-boundary).

## Appendix — Preset vectors

The exact band gain vectors for the four non-Flat presets are written against the canonical ten
centres (29, 59, 119, 237, 474, 947, 1889, 3770, 7523, 15011 Hz):

| Preset | Preamp | 29 | 59 | 119 | 237 | 474 | 947 | 1889 | 3770 | 7523 | 15011 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Flat | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| Pop | −2.0 | +1.0 | +2.0 | +3.0 | +2.0 | 0.0 | −1.0 | −1.0 | 0.0 | +1.0 | +2.0 |
| Rock | −1.0 | +3.0 | +2.0 | 0.0 | −1.0 | −1.0 | +0.0 | +2.0 | +3.0 | +3.0 | +2.0 |
| Jazz | −1.0 | +2.0 | +1.0 | 0.0 | +1.0 | +1.0 | +0.0 | +1.0 | +2.0 | +2.0 | +1.0 |
| Classical | −2.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | +1.0 | +2.0 | +3.0 |

Column headers above are the band centre frequencies in Hz. All values are linear dB and rounded
to one decimal. Any deviation from this table is a contract change and must be reflected in the
appendix before the matching code lands.
