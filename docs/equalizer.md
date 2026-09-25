# Equalizer

- Status: implemented for the local output
- Tracking issue: [#49](https://github.com/jm2/tributary/issues/49)
- Code: `src/audio/equalizer.rs` (settings and the GStreamer bin),
  `src/ui/equalizer_panel.rs` (the Preferences group)

Tributary has a ten-band graphic equalizer with Winamp's classic presets, a preamp, and optional
clip protection. Its bands, range, and layout follow iTunes and Winamp: ten octave bands from
32 Hz to 16 kHz, ±12 dB, and a row of vertical sliders. Parametric bands, room correction,
loudness normalization, and per-track or per-source profiles are out of scope.

## Controls

The **Equalizer** group sits at the bottom of Preferences:

| Control | Values |
| ------- | ------ |
| Enable equalizer | on / off (off on a fresh install) |
| Preset | Flat, Classical, Club, Dance, Full Bass, Full Bass & Treble, Full Treble, Headphones, Large Hall, Live, Party, Pop, Reggae, Rock, Ska, Soft, Soft Rock, Techno, Custom |
| Preamp | −12 to +12 dB in 0.5 dB steps |
| Ten bands | 32, 64, 125, 250, 500 Hz, 1, 2, 4, 8, 16 kHz; −12 to +12 dB in 0.5 dB steps |
| Clip protection | Off, Soft |
| Reset to Flat | sets the preset, preamp, and bands to Flat; keeps the switch and clip protection |

The preamp and the bands are one row of vertical sliders between the Preset and Clip protection
rows, as in iTunes and Winamp: the preamp first, a separator, then the ten bands from 32 Hz to
16 kHz. Up is boost. Each slider has a mark at 0 dB, the scale reads +12 dB, 0 dB, and −12 dB at
the side, and a short caption (32, 64, 125, 250, 500, 1K, 2K, 4K, 8K, 16K) sits under each band.
A slider's tooltip shows its value. For assistive technology each slider is labelled with its
name ("Preamp", "1 kHz") and reports its value as dB text ("+3.0 dB"); the captions and the scale
are presentational.

Drag a slider, or focus it and use the arrow, Page Up, and Page Down keys. The mouse wheel and
touchpad scrolling over the sliders scroll the Preferences page and never move a slider.

Choosing a named preset loads its band gains and preamp. Moving any slider by hand switches the
preset to Custom. Choosing Custom keeps the current gains. Changes apply to the playing track
straight away.

### Presets

The named presets are Winamp's classic presets. Their values come from Strawberry's
`src/equalizer/equalizer.cpp`, which carries them on a −100…100 scale over Winamp's bands at
60, 170, 310, 600 Hz and 1, 3, 6, 12, 14, 16 kHz. Tributary converts them this way:

- Each unit is 0.12 dB, for cuts and boosts alike, so ±100 is ±12 dB. (Strawberry scales cuts by
  0.24 dB, down to −24 dB.)
- Each Tributary band takes the Winamp curve's value at its centre frequency, interpolated
  linearly on a log-frequency axis. Below 60 Hz the 60 Hz value holds.
- The result is rounded to the nearest 0.5 dB.
- The preamp cuts by the preset's largest band boost, so no single band lifts a full-scale signal
  above full scale. A preset that only cuts has a 0 dB preamp. Neighbouring boosted bands overlap
  and can still add a few dB between them; Soft clip protection catches those peaks.

Winamp's "Laptop speakers/headphones" preset is called Headphones.

| Preset | Preamp | 32 | 64 | 125 | 250 | 500 | 1k | 2k | 4k | 8k | 16k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Flat | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Classical | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | −2 | −5 | −6 |
| Club | −3.5 | 0 | 0 | 0 | 1.5 | 3.5 | 3.5 | 3.5 | 3 | 1.5 | 0 |
| Dance | −6 | 6 | 6 | 4.5 | 2.5 | 0.5 | 0 | −2.5 | −4 | −5 | 0 |
| Full Bass | −8.5 | 8.5 | 8.5 | 8.5 | 8.5 | 6 | 2.5 | −2.5 | −5.5 | −6 | −6.5 |
| Full Bass & Treble | −7 | 4 | 4 | 4 | 1.5 | −3.5 | −3 | −0.5 | 3 | 6 | 7 |
| Full Treble | −10 | −6 | −6 | −6 | −6 | −4 | 2 | 5 | 8 | 9.5 | 10 |
| Headphones | −5 | 3 | 3 | 5 | 4 | −1 | 0 | −2.5 | −4 | −5 | 0 |
| Large Hall | −6 | 6 | 6 | 6 | 4.5 | 3.5 | 0 | −2 | −3 | −3 | 0 |
| Live | −3.5 | −3 | −3 | −1 | 1.5 | 3 | 3.5 | 3.5 | 3 | 2 | 1 |
| Party | −4 | 4 | 4 | 4 | 1.5 | 0 | 0 | 0 | 0 | 0 | 4 |
| Pop | −4.5 | −1 | −1 | 2 | 4 | 4.5 | 3 | 0.5 | −1 | −2 | −1 |
| Reggae | 0 | 0 | 0 | 0 | −0.5 | −3 | 0 | −2.5 | −4 | −2.5 | 0 |
| Rock | −6.5 | 5 | 4.5 | 3.5 | −1 | −4.5 | −2.5 | 0.5 | 3.5 | 6 | 6.5 |
| Ska | −6 | −2 | −2 | −2.5 | −3 | −1.5 | 2.5 | 3 | 4.5 | 5.5 | 6 |
| Soft | −7 | 3 | 3 | 1.5 | 0 | −1.5 | −0.5 | 1.5 | 3.5 | 5.5 | 7 |
| Soft Rock | −5.5 | 2.5 | 2.5 | 2.5 | 1.5 | 0 | −3 | −3.5 | −3 | −1.5 | 5.5 |
| Techno | −5.5 | 5 | 4.5 | 4 | 1.5 | −2.5 | −3 | −1 | 2 | 5.5 | 5.5 |

## Supported outputs

Only the local output runs the equalizer. AirPlay, Chromecast, and MPD receivers decode and play
the audio themselves, so Tributary's pipeline never processes what they play.
`AudioOutput::supports_equalizer` is `false` by default. Only the local output returns `true`,
and only while its equalizer is available. When it is `false`, the group shows the controls
disabled, with the reason as the group description. The settings stay saved and apply again when
you switch back to the local output.

## Filter graph

The local player builds one filter bin at startup and installs it as `playbin3`'s `audio-filter`:

```text
audioresample ! audioconvert ! capsfilter(format=F32LE, layout=interleaved)
  ! volume (preamp) ! equalizer-nbands (num-bands=10) ! rglimiter
  ! audioconvert ! audioresample
```

- The player sets `audio-filter` once, at construction, while the pipeline is still `NULL`. Each
  track load already takes the pipeline through `NULL`, and `playbin3` reuses the same bin for
  the next stream. Only the sample format is pinned. Sample rate and channel count follow each
  stream, so multichannel files are not downmixed.
- `equalizer-10bands` has fixed centres at 29 Hz…15 kHz, so the bin uses `equalizer-nbands`.
  Both elements ship in the same `equalizer` plugin. When the bin is built, each of the ten band
  objects gets:
  - `freq`: its centre, 32 Hz…16 kHz.
  - `bandwidth`: the distance from the previous band's centre, and 32 Hz for the first band.
    This is how Strawberry spaces its GStreamer bands. On octave centres it gives every band
    above the first a Q of 2.
  - `type`: `peak`. `equalizer-nbands` makes its first band a low shelf and its last a high
    shelf, and a shelf reaches only half its gain at its own frequency: a +12 dB shelf on the
    16 kHz band lifts 16 kHz by 6 dB. Strawberry keeps its ten bands off the shelves too. As peak
    filters, every band reaches the slider's gain at the frequency under the slider.
- The bin never changes shape. Every setting is a property write, and all three processing
  elements accept property writes while playing:
  - `volume` takes the preamp as a linear factor, `10^(dB/20)`.
  - Each `equalizer-nbands` band takes its `gain` in dB.
  - `rglimiter` takes `enabled`.

  Enabling, disabling, presets, slider moves, and clip protection all take effect on the next
  buffer. Playback never pauses and the bin is never relinked.
- **Disabled** writes neutral values: unity preamp, 0 dB on every band, and the limiter off.
  `equalizer-nbands` and `rglimiter` run in passthrough at those values. The conversion to
  32-bit float stays in place.
- **Soft** clip protection turns `rglimiter` on. It compresses peaks above −6 dBFS and never
  lets them exceed 0 dBFS. **Off** passes samples unchanged, so large boosts can clip at the
  output.
- `equalizer-nbands` and `rglimiter` ship in gst-plugins-good, which the deb, rpm, and Arch
  packages already require. The Windows and macOS bundles ship the `equalizer` and
  `replaygain` plugins.

## Failures

- If an element cannot be created, for example because gst-plugins-good is missing, the player
  runs without an `audio-filter`. Playback is unchanged, and the Preferences group says the
  equalizer is unavailable.
- If an element inside the bin posts an error during playback, the bus watch removes the bin
  and restarts the current stream without it. Once the stream prerolls, the watch seeks back to
  where the failure happened. The track keeps playing without the equalizer, and the group
  reports the equalizer unavailable for the rest of the session. Errors from anywhere else in
  the pipeline are handled as before.

## Persistence

The settings are stored in the `equalizer` field of `config.json`, next to the other
preferences:

```json
"equalizer": {
  "enabled": true,
  "preset": "rock",
  "preamp_db": -6.5,
  "bands_db": [5.0, 4.5, 3.5, -1.0, -4.5, -2.5, 0.5, 3.5, 6.0, 6.5],
  "clip_protection": "soft"
}
```

Preset names are saved in snake case: `flat`, `classical`, `club`, `dance`, `full_bass`,
`full_bass_treble`, `full_treble`, `headphones`, `large_hall`, `live`, `party`, `pop`, `reggae`,
`rock`, `ska`, `soft`, `soft_rock`, `techno`, and `custom`.

- Missing fields take their defaults. A config written before this field existed loads with the
  equalizer off and set to Flat.
- On load, gains are clamped to −12…+12 dB and rounded to the nearest 0.5 dB. A named preset
  whose gains no longer match it becomes `custom`. A config saved when the range reached −24 dB
  loads with those gains clamped to −12 dB.
- A preset name this version does not know, such as an earlier version's `jazz`, loads as
  `custom` and keeps its gains. A `pop`, `rock`, or `classical` saved with an earlier version's
  gains also becomes `custom`, because the gains no longer match the preset.
- A malformed `equalizer` value, such as a preset that is not a string or the wrong number of
  bands, resets only this field to its defaults. The rest of `config.json` still loads.
- The group saves through the shared atomic `save_config`. Slider drags are grouped into one
  write 750 ms after the first change, the same delay the volume slider uses. A write still
  pending when the Preferences dialog closes is saved then.

## Tests

- `src/audio/equalizer_tests.rs` covers the preset tables, validation, configs saved by earlier
  versions, and malformed configs. Real pipelines (`audiotestsrc`, the bin, `level`) check that:
  - the bands are peak filters on the ISO centres with the Strawberry widths;
  - each band reaches +6 dB and −12 dB at its own centre, 32 Hz and 16 kHz included;
  - the preamp and band gains change the measured level, and the disabled bin leaves it alone;
  - Soft clip protection keeps peaks at or below 0 dBFS;
  - property writes take effect mid-stream.

  `playbin3` tests load files with different rates and channel counts through the one installed
  bin, and recover from an injected in-bin failure. The pipeline tests skip only when the
  GStreamer elements are not installed.
- `src/ui/equalizer_panel.rs` covers label formatting, preset names, and key parity across all 13
  locale catalogs. Its GTK widget contracts run in the shared widget-test session. They check the
  slider layout and accessibility, keyboard steps, and that a scroll over the sliders moves the
  enclosing scrolled window, is stopped before the sliders, and edits nothing.
