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
| Bands 1..10     | linear dB  | `−12.0` … `0.0` … `+12.0` dB                       | 0.5 dB    | `0.0` dB   |
| Clip protection | enum       | `Off` / `Soft` (transparent limiter at −1 dBFS)    | —         | `Off`      |

Ten ISO-standard band center frequencies at integer-Q (1.0 octave) are fixed by the spec:
**31 Hz, 62 Hz, 125 Hz, 250 Hz, 500 Hz, 1 kHz, 2 kHz, 4 kHz, 8 kHz, 16 kHz.** A future change to
band count, frequencies, or Q requires a new contract revision.

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

`Enabled = false` is global bypass: the pipeline removes the EQ filter from the graph entirely
without disturbing the persisted settings, so toggling EQ back on restores the prior bands. The
default for fresh installs is `false`.

## Filter graph

All equalizer DSP runs in the local-output `playbin3` pipeline by inserting a single `equalizer-10bands`
filter between decoder and sink. The filter accepts the ten band gains in dB, an overall preamp in
dB, and a boolean bypass flag. The filter implements its own internal biquad cascade (10 biquads
for the bands + 1 gain stage for the preamp) and short-circuits on bypass without re-instantiating
the filter element.

The chain layout for the *enabled* state is:

```
uridecodebin ! audioconvert !
    equalizer-10bands name=eq preamp=−N dB
        band0=... band1=... ... band9=... !
    audioamplify name=clipper amplification=1.0 clip-mode=1 max-amplitude=0.891 !
    audioconvert ! playsink
```

Where:

- `equalizer-10bands` is the canonical `gst-plugins-good` element. The plugin name, the property
  spelling (`band0`…`band9`, `preamp`), and the short-circuit bypass behavior are deliberate —
  substituting a different filter element is a contract change.
- `audioamplify` with `clip-mode=1` (soft-knee) and `max-amplitude=0.891` (≈ −1.0 dBFS) is the
  clip-protection limiter. It is gated by the `Clip protection` setting. When the setting is
  `Off`, the element is removed from the pipeline and `clip-mode` is `0` (off, no soft-knee and
  no envelope tracking; the `clip-mode` value is then `0` and `max-amplitude` is ignored).
- The `audioconvert` immediately before the EQ element is the same `audioconvert` already used to
  conform sample rate and channel layout for the EQ filter's biquads.
- The post-EQ `audioamplify` is not present when clip protection is `Off`; the pipeline reduces
  to:
  ```
  uridecodebin ! audioconvert ! equalizer-10bands ! audioconvert ! playsink
  ```
- `playsink` is the existing `playbin3`-managed audio sink. The equalizer never reaches the
  receiver-side path; AirPlay, Chromecast, and MPD outputs do not have an `equalizer-10bands`
  filter and never will under this contract.

`equalizer-10bands` runs at 32-bit floating-point internally. The pre-EQ `audioconvert` is the
default configuration that the existing pipeline already negotiates; the equalizer-10bands
element's caps accept any PCM layout the rest of the pipeline produces. The post-EQ `audioconvert`
downconverts to the sink caps. The pipeline continues to honor gapless navigation: the
`equalizer-10bands` properties are set before the URI transitions to `Playing` and `playbin`'s gapless
event hook only re-applies volume and event generation, not EQ settings.

## Band and preamp mechanics

Each band implements a peak-filter biquad whose center frequency, Q, and gain are properties of
the element. The element's gain is set in dB and applied to the biquad coefficients in real time;
the filter is therefore a *fixed-Q* parametric, not an arbitrary IIR. Ten integer band centers
(31 Hz through 16 kHz) are used; bands are written as `band0` (31 Hz) through `band9` (16 kHz).

The preamp is a single linear gain applied before the band stack. Selecting a named preset
overwrites *all ten* band gains *and* the preamp in a single atomic property write sequence. The
UI must show the new preset name as the active preset; it must not allow manual band edits to
silently rename the preset.

Manual edits to the band vector always begin from the current band vector; selecting a different
preset again is required to switch back to a named response. The contract does not preserve a
"modified" flag in storage; the active preset name is the source of truth.

`Clip protection = Soft` inserts a single-stage soft-knee peak limiter immediately after the EQ
stage. The limiter uses `audioamplify`'s built-in envelope follower with `clip-mode=1` and
`max-amplitude=0.891` so that the post-filter peak never exceeds approximately −1.0 dBFS.
Limiter attack and release are fixed by the element; no user-visible controls are exposed.

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
| `Enabled`              | yes           | Atomic element insert / remove on the playing pipeline |
| `Preset`               | yes           | Atomic ten-band + preamp property write              |
| `Preamp`               | yes           | Single property write on the element                |
| Single band `bandN`    | yes           | Single property write on the element                |
| Multiple bands at once | yes           | Batched property writes, delivered as one transition|
| `Clip protection`      | yes           | Pipeline mute → element insert/remove → unmute      |
| Band Q / frequencies   | NO            | Frozen by the spec; changing requires a new contract |

Live reconfiguration is delivered through `playbin` element property writes (`g_object_set`) so
that the element's internal coefficients update on the next audio buffer without a pipeline
state transition. No re-link, no re-instantiate, no seek, no EOS-resending. A `Buffering` event
may be observed during the bus-flush the property write produces; the UI may briefly show a
spinner, but playback must continue to advance position.

`Clip protection` is the exception, because adding or removing a downstream element requires an
upstream link reseating. The element insert/remove is gated by:
1. Pause the pipeline (`gst::State::Paused`),
2. Insert or remove the limiter element,
3. Link the new chain (`audioconvert` ↔ limiter ↔ `audioconvert`),
4. Re-enter `Playing`,
5. Mark the change in metrics as a brief swap (≤ 100 ms by spec).

A pause/resume swap is undesirable but is the only correct option for a downstream element
topology change. The contract accepts this cost because it is paid only when `Clip protection`
toggles, which the user is not expected to do frequently.

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
- `bands_db` values outside `[-12.0, +12.0]` are clamped to the boundary.
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
| Local            | supported     | Pipeline owns the decoder-to-sink chain; `equalizer-10bands` runs in process.                                              |
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

- A single informational log message is emitted when the equalizer filter is inserted into or
  removed from the local-output pipeline. The log carries boolean enabled state and the active
  preset name. It never carries individual band values, preamp values, or file paths.
- A single informational log message is emitted when the clip-protection limiter is inserted
  into or removed from the local-output pipeline. It carries the boolean clip-protection state.
- The `equalizer.cfg` file path is logged at debug only, never at info or above.
- Diagnostic state on a malformed file is emitted at warn with the file path, byte count, and
  bad key only. The user's prior preferences are not dumped; the file content is not dumped.
- EQ metrics (e.g. peak amplitude per band, average gain) are deliberately not exposed in the
  UI or logs. Surfacing them would invite user-visible polish without a contract owner.

## Accessibility and localization

The settings UI shall advertise each equalizer control with its accessible label and the
keyboard accelerator that increases and decreases the value. Numeric bands must announce their
current value, the unit (decibels), and the boundary (e.g. `−6.0 dB, range minus twelve to
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
| Fresh install, EQ disabled                              | Persisted state matches the default; equalizer is bypassed in the pipeline |
| Enable EQ on local output                               | `equalizer-10bands` inserted; pipeline remains in `Playing`               |
| Change a single band mid-playback                       | Buffer passes; no gapless discontinuity; new value reaches the filter      |
| Select Pop preset mid-playback                          | All ten bands + preamp updated atomically; preset name updates in UI      |
| Cycle clip protection Off → Soft → Off                   | Pause/resume swap each time; total swap ≤ 100 ms per toggle              |
| Switch active output to AirPlay                         | EQ module renders disabled in UI; no equalizer runs on the RAOP pipeline  |
| Switch active output back to Local                      | EQ module renders enabled in UI if previously enabled; pipeline re-attaches the filter |
| Quit while a slider drag is in progress                 | Last debounced write is persisted; no partial writes                       |
| Malformed `equalizer.cfg` on disk                       | Replaced with defaults; single warn-level diagnostic published             |
| Preamp outside bounds in saved file                     | Value clamped to range; preset and bands remain valid                     |
| Band value outside bounds in saved file                 | Value clamped to range; other bands remain valid                          |
| Preset name not in the named set on disk                | Coerced to `Flat`; band vector becomes all zeros                          |
| Hardware sink with 8-channel layout (macOS)             | Pre-EQ `audioconvert` caps remain `[1, 2]`; EQ runs in stereo; same cap fix as existing module |
| Gapless album transition                                | EQ settings re-applied to the new generation; no audible difference       |

## Implementation boundary

The implementation record listed in `task.md` P2.4 line 1006 lands this contract. The record is
intentionally bounded: implement the supported path and the disabled-UI path, then validate the
acceptance matrix. Implementation is **not** covered by this design document; the design document
is the source of truth for the contract and changes only by revision.

The implementation record:

- Adds the `equalizer-10bands` filter wiring to the local pipeline.
- Adds the clip-protection limiter wiring behind a single boolean.
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

The exact band gain vectors for the four non-Flat presets are:

| Preset    | Preamp  | 31 Hz | 62 Hz | 125 Hz | 250 Hz | 500 Hz | 1 kHz | 2 kHz | 4 kHz | 8 kHz | 16 kHz |
|-----------|---------|-------|-------|--------|--------|--------|-------|-------|-------|-------|--------|
| Flat      |  0.0    | 0.0   | 0.0   | 0.0    | 0.0    | 0.0    | 0.0   | 0.0   | 0.0   | 0.0   | 0.0    |
| Pop       | −2.0    | +1.0  | +2.0  | +3.0   | +2.0   | 0.0    | −1.0  | −1.0  | 0.0   | +1.0  | +2.0   |
| Rock      | −1.0    | +3.0  | +2.0  | 0.0    | −1.0   | −1.0   | +0.0  | +2.0  | +3.0  | +3.0  | +2.0   |
| Jazz      | −1.0    | +2.0  | +1.0  | 0.0    | +1.0   | +1.0   | +0.0  | +1.0  | +2.0  | +2.0  | +1.0   |
| Classical | −2.0    | 0.0   | 0.0   | 0.0    | 0.0    | 0.0    | 0.0   | 0.0   | +1.0  | +2.0  | +3.0   |

All values are linear dB and rounded to one decimal. Any deviation from this table is a contract
change and must be reflected in the appendix before the matching code lands.
