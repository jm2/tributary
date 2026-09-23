# Equalizer

- Status: implemented for the local output
- Tracking issue: [#49](https://github.com/jm2/tributary/issues/49)
- Code: `src/audio/equalizer.rs` (settings and the GStreamer bin),
  `src/ui/equalizer_panel.rs` (the Preferences group)

Tributary has a ten-band graphic equalizer with presets, a preamp, and optional clip protection.
Parametric bands, room correction, loudness normalization, and per-track or per-source profiles
are out of scope.

## Controls

The **Equalizer** group sits at the bottom of Preferences:

| Control | Values |
| ------- | ------ |
| Enable equalizer | on / off (off on a fresh install) |
| Preset | Flat, Pop, Rock, Jazz, Classical, Custom |
| Preamp | −24 to +12 dB in 0.5 dB steps |
| Ten bands | 29, 59, 119, 237, 474, 947 Hz, 1.9, 3.8, 7.5, 15 kHz; −24 to +12 dB in 0.5 dB steps |
| Clip protection | Off, Soft |
| Reset to Flat | sets the preset, preamp, and bands to Flat; keeps the switch and clip protection |

Choosing a named preset loads its band gains and preamp. Moving any slider by hand switches the
preset to Custom. Choosing Custom keeps the current gains. Changes apply to the playing track
straight away.

| Preset | Preamp | 29 | 59 | 119 | 237 | 474 | 947 | 1.9k | 3.8k | 7.5k | 15k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Flat | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Pop | −2 | 1 | 2 | 3 | 2 | 0 | −1 | −1 | 0 | 1 | 2 |
| Rock | −1 | 3 | 2 | 0 | −1 | −1 | 0 | 2 | 3 | 3 | 2 |
| Jazz | −1 | 2 | 1 | 0 | 1 | 1 | 0 | 1 | 2 | 2 | 1 |
| Classical | −2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 2 | 3 |

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
  ! volume (preamp) ! equalizer-10bands ! rglimiter
  ! audioconvert ! audioresample
```

- The player sets `audio-filter` once, at construction, while the pipeline is still `NULL`. Each
  track load already takes the pipeline through `NULL`, and `playbin3` reuses the same bin for
  the next stream. Only the sample format is pinned. Sample rate and channel count follow each
  stream, so multichannel files are not downmixed.
- The bin never changes shape. Every setting is a property write, and all three processing
  elements accept property writes while playing:
  - `volume` takes the preamp as a linear factor, `10^(dB/20)`.
  - `equalizer-10bands` takes `band0`…`band9` in dB.
  - `rglimiter` takes `enabled`.

  Enabling, disabling, presets, slider moves, and clip protection all take effect on the next
  buffer. Playback never pauses and the bin is never relinked.
- **Disabled** writes neutral values: unity preamp, 0 dB on every band, and the limiter off.
  `equalizer-10bands` and `rglimiter` run in passthrough at those values. The conversion to
  32-bit float stays in place.
- **Soft** clip protection turns `rglimiter` on. It compresses peaks above −6 dBFS and never
  lets them exceed 0 dBFS. **Off** passes samples unchanged, so large boosts can clip at the
  output.
- `equalizer-10bands` and `rglimiter` ship in gst-plugins-good, which the deb, rpm, and Arch
  packages already require.

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
  "preamp_db": -1.0,
  "bands_db": [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
  "clip_protection": "soft"
}
```

- Missing fields take their defaults. A config written before this field existed loads with the
  equalizer off and set to Flat.
- On load, gains are clamped to −24…+12 dB and rounded to the nearest 0.5 dB. A named preset
  whose gains no longer match it becomes `custom`.
- A malformed `equalizer` value, such as an unknown preset or the wrong number of bands, resets
  only this field to its defaults. The rest of `config.json` still loads.
- The group saves through the shared atomic `save_config`. Slider drags are grouped into one
  write 750 ms after the first change, the same delay the volume slider uses. A write still
  pending when the Preferences dialog closes is saved then.

## Tests

- `src/audio/equalizer_tests.rs` covers the preset tables, validation, and malformed configs.
  Real pipelines (`audiotestsrc`, the bin, `level`) check that:
  - the preamp and band gains change the measured level, and the disabled bin leaves it alone;
  - Soft clip protection keeps peaks at or below 0 dBFS;
  - property writes take effect mid-stream.

  `playbin3` tests load files with different rates and channel counts through the one installed
  bin, and recover from an injected in-bin failure. The pipeline tests skip only when the
  GStreamer elements are not installed.
- `src/ui/equalizer_panel.rs` covers label formatting and key parity across all 13 locale
  catalogs. Its GTK widget contracts run in the shared widget-test session.
