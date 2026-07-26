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

Tributary offers a single ten-band parametric-style equalizer with five named presets, a global
preamp, and a hard limiter to prevent inter-sample clipping. The equalizer runs in the **local**
pipeline only. AirPlay (RAOP), Chromecast, and MPD outputs are explicitly listed with their
supported, partially supported, or unsupported status and the user-visible behavior for each
non-supported state. The settings UI surfaces the equalizer unconditionally; for unsupported
outputs the controls are rendered disabled with a closed-form explanation.

Future work (multi-band parametric, room correction, per-track profiles, per-source overrides,
lossy-format adaptive EQ, LUFS/R128 loudness normalization) requires a refined implementation
record and is not addressed by this contract.

## Bounded user surface

The equalizer exposes exactly the following controls. Each control has one bounded type, a fixed
range, a fixed precision, and a fixed default. Settings outside these bounds are invalid input and
the settings UI must reject them at the boundary; runtime code must not be able to materialize
them.

| Control         | Type       | Range / values                                     | Precision | Default    |
|-----------------|------------|----------------------------------------------------|-----------|------------|
| Enabled         | bool       | `true` / `false` (global bypass)                   | —         | `false`    |
| Preset          | enum       | `Flat` / `Pop` / `Rock` / `Jazz` / `Classical`     | —         | `Flat`     |
| Preamp          | linear dB  | `−24.0` … `0.0` … `+12.0` dB; integer or half-step | 0.5 dB    | `0.0` dB   |
| Bands 1..10     | linear dB  | `−24.0` … `0.0` … `+12.0` dB                       | 0.5 dB    | `0.0` dB   |
| Clip protection | enum       | `Off` / `Soft` (transparent limiter at −1 dBFS)    | —         | `Off`      |

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
recommended value. Switching presets must record that the present vector now describes the new
preset, not a manual edit of the previous one.

| Preset      | Recommended preamp | Description                                                       |
|-------------|--------------------|-------------------------------------------------------------------|
| Flat        | 0.0 dB             | All bands at 0.0 dB (the default state and the empty baseline)    |
| Pop         | −2.0 dB            | Bass shelf + slight mid cut + presence lift                       |
| Rock        | −1.0 dB            | Bass + treble lift, mid dip                                       |
| Jazz        | −1.0 dB            | Bass + mid lift                                                    |
| Classical   | −2.0 dB            | Slight treble lift, flat low end                                   |

Pop/Rock/Jazz/Classical gain vectors are documented in the appendix of this contract and are not
free-form; reviewers must reject any PR that introduces a new band value without the matching
entry in the appendix.

`Clip protection` is a global safety feature independent of the equalizer engine. When `Soft` is
enabled, the engine inserts a single peak limiter immediately after the equalizer stage so that
loud passages cannot exceed −1 dBFS after preamp+EQ. The limiter is transparent and adds no
user-visible control. When `Off`, no limiter exists in the pipeline and clipping can occur; the
state is preserved for power users who explicitly want raw output.

`Enabled = false` is global bypass: the equalizer filter (and its preamp stage) is *not inserted*
in the local-output pipeline at all, so the pipeline reduces to the existing passthrough chain
without disturbing the persisted settings. Toggling `Enabled` back to `true` re-inserts the
equalizer stage with the persisted bands and preamp intact. The default for fresh installs is
`false`, and a fresh install therefore does not touch the existing pipeline shape at all.

## Filter graph

All equalizer DSP runs in the local-output `playbin3` pipeline by inserting a chain of three
elements between decoder and sink: a preamp `volume` element, the canonical
`equalizer-10bands` filter, and an optional `rglimiter` peak limiter. None of these elements is
substituted for a different one without a contract change.

The chain layout for the *enabled, clip-protection-on* state is:

```
uridecodebin ! audioconvert !
    volume name=eq-preamp volume=<factor> !
    equalizer-10bands name=eq
        band0=<gain> band1=<gain> ... band9=<gain> !
    rglimiter name=clipper enabled=true !
    audioconvert ! playsink
```

Where:

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
  behavioural property is `enabled` (Boolean, default `true`); when enabled it applies a fixed
  brick-wall ceiling at approximately −1 dBFS, which is the contract's "transparent limiter at
  −1 dBFS". The element takes no other tuning; attack, release, and ceiling are fixed by the
  element. Clip protection is the second of the five deliverables named by the bead; the
  contract pins it to `rglimiter` rather than to `audioamplify`, which is not a limiter (it is a
  static amplifier with hard-clip / wrap / none options, no envelope follower, and no
  `max-amplitude` property).
- `audioconvert` immediately before the preamp element normalises sample rate and channel layout
  for the biquad cascade that follows. The post-limiter `audioconvert` downconverts to the sink
  caps. Both `audioconvert` instances are the same `audioconvert` the existing pipeline already
  negotiates.

The chain layout for the *enabled, clip-protection-off* state is:

```
uridecodebin ! audioconvert !
    volume name=eq-preamp volume=<factor> !
    equalizer-10bands name=eq
        band0=<gain> band1=<gain> ... band9=<gain> !
    audioconvert ! playsink
```

The `rglimiter` element is removed from the graph; its `enabled` flag is irrelevant because the
element is not in the chain.

The chain layout for the *disabled* state is the existing pipeline, untouched:

```
uridecodebin ! audioconvert ! playsink
```

The preamp `volume`, `equalizer-10bands`, and `rglimiter` are all absent. The persisted bands,
preamp, and clip-protection setting are dormant in this state but preserved on disk, so toggling
`Enabled` back to `true` re-inserts the chain with the prior configuration.

`equalizer-10bands` runs at 32-bit floating-point internally. The pre-EQ `audioconvert` is the
default configuration that the existing pipeline already negotiates; the `equalizer-10bands`
element's caps accept any PCM layout the rest of the pipeline produces. `rglimiter` requires
`F32LE`; the post-EQ `audioconvert` (when the limiter is in the chain) is responsible for
negotiating that format. The pipeline continues to honor gapless navigation: the
`equalizer-10bands` and preamp `volume` properties are set before the URI transitions to
`Playing`, and `playbin`'s gapless event hook only re-applies volume and event generation, not
EQ settings.

The equalizer never reaches the receiver-side path; AirPlay, Chromecast, and MPD outputs do not
have an equalizer chain and never will under this contract.

## Band and preamp mechanics

Each band implements a peak-filter biquad whose center frequency, Q, and gain are properties of
the element. The element's gain is set in dB and applied to the biquad coefficients in real time;
the filter is therefore a *fixed-Q* parametric, not an arbitrary IIR. The ten canonical band
centres (29 Hz through 15011 Hz) are the element's own; bands are written as `band0` (29 Hz)
through `band9` (15011 Hz).

The preamp is a single linear gain applied by the dedicated `volume` element *before* the band
stack; selecting a named preset overwrites *all ten* band gains *and* the preamp in a single
atomic property write sequence. The UI must show the new preset name as the active preset; it
must not allow manual band edits to silently rename the preset.

Manual edits to the band vector always begin from the current band vector; selecting a different
preset again is required to switch back to a named response. The contract does not preserve a
"modified" flag in storage; the active preset name is the source of truth.

`Clip protection = Soft` inserts `rglimiter enabled=true` immediately after the EQ stage. The
limiter's ceiling is fixed by the element at approximately −1 dBFS. Limiter attack, release, and
ceiling are fixed by the element; no user-visible controls are exposed.

Pre-LP clip behavior is what clip protection actually guards against: the EQ can elevate peaks
above 0 dBFS even with a sane-looking preamp, especially on already-mastered pop/rock material.
When `Clip protection = Off`, the contract explicitly permits clipping and the application must
not pretend it was prevented. The `Soft` option is therefore the recommended default for fresh
installs starting with enabled EQ.

## Live-reconfiguration boundary

The contract intentionally limits which knobs can change mid-playback and at which pipeline stage
the changes take effect. The boundary is:

| Knob                   | Mid-playback? | Mechanism                                            |
|------------------------|---------------|-----------------------------------------------------|
| `Enabled`              | yes           | Pause → chain insert/remove → resume                 |
| `Preset`               | yes           | Atomic ten-band property write + preamp property write |
| `Preamp`               | yes           | Single property write on the preamp `volume` element |
| Single band `bandN`    | yes           | Single property write on the `equalizer-10bands` element |
| Multiple bands at once | yes           | Batched property writes, delivered as one transition |
| `Clip protection`      | yes           | Pause → `rglimiter` insert/remove → resume           |
| Band centres / Q       | NO            | Frozen by the spec; changing requires a new contract |

Live reconfiguration of `Enabled` flips the entire equalizer chain (preamp `volume` +
`equalizer-10bands`, with or without `rglimiter`) between *present-in-the-graph* and
*absent-from-the-graph*. Because the chain includes upstream and downstream elements of the
existing passthrough, the toggle requires the same pause/relink seam as `Clip protection`:

1. Pause the pipeline (`gst::State::Paused`),
2. Insert or remove the equalizer chain elements as one edit (preamp `volume`,
   `equalizer-10bands`, and `rglimiter` if `Clip protection = Soft`),
3. Re-link the new chain (`audioconvert` ↔ preamp `volume` ↔ `equalizer-10bands` ↔
   `rglimiter` ↔ `audioconvert` when enabled, or directly `audioconvert` ↔ `audioconvert` when
   disabled),
4. Re-enter `Playing`,
5. Mark the change in metrics as a brief swap (≤ 100 ms by spec).

The persisted bands, preamp, and clip-protection setting are not touched on either side of the
transition.

Live reconfiguration of band gains, the preamp, and the preset is delivered through property
writes (`g_object_set`) on the running elements so that coefficients update on the next audio
buffer without a pipeline state transition. No re-link, no re-instantiate, no seek, no
EOS-resending. A `Buffering` event may be observed during the bus-flush the property write
produces; the UI may briefly show a spinner, but playback must continue to advance position.

`Clip protection` toggles the `rglimiter` element's *presence* in the graph (insert or remove)
using the same pause/relink seam:

1. Pause the pipeline (`gst::State::Paused`),
2. Insert or remove the `rglimiter` element,
3. Re-link the new chain (`equalizer-10bands` ↔ `rglimiter` ↔ `audioconvert`, or directly
   `equalizer-10bands` ↔ `audioconvert` when the limiter is removed),
4. Re-enter `Playing`,
5. Mark the change in metrics as a brief swap (≤ 100 ms by spec).

A pause/resume swap is undesirable but is the only correct option for a topology change of
either kind. The contract accepts this cost because it is paid only on user-initiated
`Enabled` or `Clip protection` toggles, neither of which the user is expected to perform
frequently.

When the playing track is a live stream with no `gapless` table (e.g. a remote radio URL), element
insert/remove must not disturb the upstream decoder's buffering; the same pause/insert/resume
seam is used and a one-time metadata-free "reconfiguring audio output" diagnostic is published.
A failure to re-attach the limiter is treated as a recoverable error: the chain degrades to
the no-limiter layout and the user-visible status becomes the same as `Clip protection = Off`.

## Persistence

Persisted equalizer state is six values:

| Field            | Type                  | Storage                                                                 |
|------------------|-----------------------|-------------------------------------------------------------------------|
| `enabled`        | bool                  | `equalizer.cfg` key `enabled`                                            |
| `preset`         | enum (named string)   | `equalizer.cfg` key `preset`                                             |
| `preamp_db`      | f32 in range `[−24, +12]` | `equalizer.cfg` key `preamp_db` (formatted to one decimal)         |
| `bands_db`       | `[f32; 10]` per bounds | `equalizer.cfg` keys `band0_db`…`band9_db` (one decimal each)         |
| `clip_protect`   | enum (`off` / `soft`) | `equalizer.cfg` key `clip_protect`                                      |
| `schema_version` | u32 = `1`             | `equalizer.cfg` key `schema_version`                                    |

`equalizer.cfg` lives in the existing `dirs::data_dir()/tributary/` directory beside
`volume`. The on-disk format is one line per key, comments are not permitted, key order is
stable (the keys above, in that order), and values are quoted with `"…"` so that whitespace,
quotes, or unicode values cannot break parsing.

Persistence uses the same debounced single-writer pattern already used by `Player::save_volume`.
A 750 ms idle interval coalesces slider-drag changes into one write per change-spell. The save
runs on the GTK main loop and is suppressed entirely when the `Enabled = false` state matches
the default, the preset is `Flat`, all bands are zero, preamp is zero, and clip protection is
`Off`.

Fresh-install default state is exactly:

```ini
schema_version=1
enabled=false
preset=flat
preamp_db=0.0
band0_db=0.0
…
band9_db=0.0
clip_protect=off
```

The `dirs::data_dir()/tributary/equalizer.cfg` file is owned by the equalizer module; no other
module reads or writes it. Migration from prior versions is out of scope; any pre-existing
equalizer file from a different schema is replaced with the default state on first load.

Validation rules on read:

- `schema_version` must equal `1`; any other value replaces the file with defaults.
- All listed keys must be present; a missing key becomes the default for that key.
- `bands_db` values outside `[-24.0, +12.0]` are clamped to the boundary.
- `preamp_db` outside `[-24.0, +12.0]` is clamped to the boundary.
- `preset` outside the named set becomes `Flat`.
- `enabled` not parseable as bool becomes `false`.
- `clip_protect` outside `off` / `soft` becomes `off`.

A malformed file is replaced with the default state, the user's prior preferences are recorded
in a typed diagnostic with file path, byte count, and the bad key, and the change is not silent.

## Capability matrix

The capability matrix below assigns a fixed status to each output for equalizer DSP. The status
is communicated to the UI through a new trait method, and the rendering is fixed per status; a
shipped output cannot claim an unsupported status, and a future implementation that adds a
status must ship the matching UI rendering at the same time.

| Output           | Equalizer DSP | Reasoning                                                                                                                  |
|------------------|---------------|----------------------------------------------------------------------------------------------------------------------------|
| Local            | supported     | Pipeline owns the decoder-to-sink chain; the equalizer chain runs in process.                                              |
| AirPlay (RAOP)   | unsupported   | The receiving speaker renders audio; in-band equalizer protocol is proprietary and not exposed by the deployed receiver APIs. |
| Chromecast       | unsupported   | The receiving speaker renders audio; the Cast V2 protocol does not expose a public equalizer channel.                       |
| MPD              | unsupported   | MPD exposes server-side EQ commands (`eq`, `setvol`) that require server cooperation and vary by `libmpdclient` build; the canonical contracted behavior is host-side rendering, so host EQ does not reach the receiver. |

For each `unsupported` output, the user-visible settings UI renders the equalizer controls as
disabled with a tool-tip explaining the limitation (e.g. "AirPlay receivers render audio
end-to-end, so Tributary's equalizer cannot reach the speaker.") Disabled controls preserve the
last-saved values locally; the equalizer does run for the local output even while the user's
active output is unsupported, so a later switch back to local reflects the same persisted bands.

If the active output is unsupported, the equalizer is *not* applied to anything — the
local-output pipeline is not the active pipeline, so re-running the filter on the local sink
is ignored by the application. The persisted equalizer is dormant in this state, not lost.

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
five named values listed above.

The settings UI is localized in the same locale set as the rest of the application
(`locales/en.yml`, `de.yml`, `es.yml`, `fr.yml`, `it.yml`, `ja.yml`, `ko.yml`, `nl.yml`,
`pl.yml`, `pt-BR.yml`). The five preset names are translated, but the keys stored in
`equalizer.cfg` remain English (`flat`, `pop`, `rock`, `jazz`, `classical`). The migration of
older non-English keys is not expected; an unknown preset value is treated as `Flat`.

## Acceptance matrix

Implementation acceptance requires the exact conditions listed below. The matrix is exhaustive
for this contract; new conditions require a new revision.

| Scenario                                                | Expected outcome                                                          |
|---------------------------------------------------------|---------------------------------------------------------------------------|
| Fresh install, EQ disabled                              | Persisted state matches the default; equalizer chain is absent from the pipeline |
| Enable EQ on local output                               | Preamp `volume` + `equalizer-10bands` (+ `rglimiter` if `Clip protection=Soft`) inserted via pause/relink seam; pipeline returns to `Playing`; total swap ≤ 100 ms |
| Change a single band mid-playback                       | Buffer passes; no gapless discontinuity; new value reaches the filter      |
| Select Pop preset mid-playback                          | All ten bands + preamp updated atomically; preset name updates in UI      |
| Cycle clip protection Off → Soft → Off                   | Pause/resume swap each time; total swap ≤ 100 ms per toggle              |
| Switch active output to AirPlay                         | EQ module renders disabled in UI; no equalizer chain runs on the RAOP pipeline |
| Switch active output back to Local                      | EQ module renders enabled in UI if previously enabled; pipeline re-attaches the chain |
| Quit while a slider drag is in progress                 | Last debounced write is persisted; no partial writes                       |
| Malformed `equalizer.cfg` on disk                       | Replaced with defaults; single warn-level diagnostic published             |
| Preamp outside bounds in saved file                     | Value clamped to range; preset and bands remain valid                     |
| Band value outside bounds in saved file                 | Value clamped to range; other bands remain valid                          |
| Preset name not in the named set on disk                | Coerced to `Flat`; band vector becomes all zeros                          |
| Hardware sink with 8-channel layout (macOS)             | Pre-EQ `audioconvert` caps remain `[1, 2]`; EQ runs in stereo; same cap fix as existing module |
| Gapless album transition                                | EQ settings re-applied to the new generation; no audible difference       |
| Preamp `0.0 dB` selected                                | Preamp `volume` element is in the chain with `volume=1.0`; gain flat       |

## Implementation boundary

The implementation record listed in `task.md` P2.4 line 1006 lands this contract. The record is
intentionally bounded: implement the supported path and the disabled-UI path, then validate the
acceptance matrix. Implementation is **not** covered by this design document; the design document
is the source of truth for the contract and changes only by revision.

The implementation record:

- Adds the preamp `volume` + `equalizer-10bands` (+ optional `rglimiter`) chain wiring to the
  local pipeline, with all three elements' spellings (`volume name=eq-preamp volume=<factor>`,
  `equalizer-10bands name=eq band0=<gain> ... band9=<gain>`,
  `rglimiter name=clipper enabled=true`) as specified in *Filter graph* above.
- Adds the `rglimiter` insert/remove behind the single `Clip protection` boolean.
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

| Preset    | Preamp  | 29 Hz | 59 Hz | 119 Hz | 237 Hz | 474 Hz | 947 Hz | 1889 Hz | 3770 Hz | 7523 Hz | 15011 Hz |
|-----------|---------|-------|-------|--------|--------|--------|--------|---------|---------|---------|----------|
| Flat      |  0.0    | 0.0   | 0.0   | 0.0    | 0.0    | 0.0    | 0.0    | 0.0     | 0.0     | 0.0     | 0.0      |
| Pop       | −2.0    | +1.0  | +2.0  | +3.0   | +2.0   | 0.0    | −1.0   | −1.0    | 0.0     | +1.0    | +2.0     |
| Rock      | −1.0    | +3.0  | +2.0  | 0.0    | −1.0   | −1.0   | +0.0   | +2.0    | +3.0    | +3.0    | +2.0     |
| Jazz      | −1.0    | +2.0  | +1.0  | 0.0    | +1.0   | +1.0   | +0.0   | +1.0    | +2.0    | +2.0    | +1.0     |
| Classical | −2.0    | 0.0   | 0.0   | 0.0    | 0.0    | 0.0    | 0.0    | 0.0     | +1.0    | +2.0    | +3.0     |

All values are linear dB and rounded to one decimal. Any deviation from this table is a contract
change and must be reflected in the appendix before the matching code lands.