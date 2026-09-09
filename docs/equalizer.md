# Equalizer contract

- Status: design only — P2.4 equalizer design record; implementation pending
- Decision date: 2026-07-24
- Tracking issue: [#49](https://github.com/jm2/tributary/issues/49)
- Backlog entry: [`task.md`](task.md) (P2.4)
- Roadmap rationale: [`roadmap.md`](roadmap.md) (Audio-output plan, item 1)

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
| --- | --- | --- | --- | --- |
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
| --- | --- | --- |
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

The bin exposes two ghost pads, `audio-filter-sink` (into the leading `audioresample`) and
`audio-filter-src` (out of the trailing `audioresample`); `playbin3` links them like any other
element's pads. The format boundary is the *internal pre-EQ `capsfilter`*, not a property of
the bin: a plain `GstBin` has no `caps` property, and this contract does not pretend otherwise.
The `capsfilter` pins `audio/x-raw, format=F32LE, channels=2, layout=interleaved` with the
sample rate left free — the rate is fixed once, by ordinary pad negotiation across the ghost
pads at chain-construction time, and that one rate then holds end-to-end inside the bin.
Because the leading `audioresample` and `audioconvert` accept any raw PCM, negotiation
succeeds for every PCM source regardless of its native rate, channel count, or format; the
only negotiation failure that reaches the rollback path is non-PCM upstream (typically a
malformed source): the bin's ghost pads never agree on caps, `playbin3` posts the error on the
bus, the implementation removes the bin, and the pipeline falls back to the existing
passthrough layout (a single info-level diagnostic names the source URI). This is the spec's
only rollback path: there is no element-by-element fallback inside the bin, and a failed
negotiation does not leave the chain half-inserted.

Only equalizer-originated GStreamer failures retire the equalizer chain. A bus error whose
message source lies inside `eq-bin` — or a negotiation failure of the bin's ghost pads —
triggers the passthrough fallback above; errors originating anywhere else (decoder, demuxer,
network, or audio sink) leave the bin installed and the equalizer state untouched. The
equalizer module must not interpret unrelated playback failures as equalizer failures and must
not remove the chain in reaction to them.

The chain layout for the *enabled, clip-protection-on* state is:

```text
playbin3.audio-filter (bin "eq-bin") {
    audioresample !
    audioconvert !
    capsfilter caps="audio/x-raw,format=F32LE,channels=2,layout=interleaved" !
    volume            name=eq-preamp  volume=<factor> !
    equalizer-10bands name=eq
        band0=<gain> band1=<gain> ... band9=<gain> !
    rglimiter         name=clipper    enabled=true !
    audioconvert !
    audioresample
} !
playbin3.audio-sink
```

The `rglimiter` row is present only in the clip-protection-on state (see below).

Where:

- `audioresample` (`gst-plugins-base`) is the sample-rate converter. Two instances live in the
  chain, one at each edge of the bin. The leading instance converts the upstream decoder's
  native rate to the bin's single negotiated rate; the trailing instance exists only so the
  bin's src ghost pad can negotiate freely against the audio sink — if the sink requires a
  rate different from the one pinned inside the bin, the trailing instance converts on the
  way out. Neither `equalizer-10bands` nor `rglimiter` changes rate, channel count, or format:
  the rate that enters the EQ stage is the rate that leaves it. The
  trailing pair adapts to the sink side; it compensates for nothing inside the bin.
- `audioconvert` (`gst-plugins-base`) is the format/channel converter. Two instances flank the
  EQ stage: one before the EQ (to bring arbitrary decoder output to the format/channel layout
  the `capsfilter` pins) and one after (to convert to whatever the audio sink accepts on the
  way out). `audioconvert` performs conversion work; it pins nothing.
- `capsfilter` (`gst-plugins-base`) pins audio caps on a pad. The single instance sits pre-EQ
  and pins the format the biquad cascade and the limiter are both verified against
  (`audio/x-raw, format=F32LE, channels=2, layout=interleaved`, rate free). There is no
  post-EQ `capsfilter`: the trailing `audioconvert`/`audioresample` pair negotiates the sink
  side freely. This is the only element in the chain that *pins* a format; it is the format
  boundary of the whole feature.
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

`equalizer-10bands` runs at 32-bit floating-point internally. The pre-EQ `capsfilter` pins
`audio/x-raw, format=F32LE, channels=2, layout=interleaved` so the biquad cascade and the
limiter both see the format they are verified against. Downstream of the limiter, the trailing
`audioconvert`/`audioresample` pair negotiates freely with whatever the audio sink requires —
a different signedness, channel count, or rate — so the sink side is never constrained to
F32LE. `playbin3`'s gapless navigation keeps the same bin installed across URI transitions;
the equalizer state is not re-applied automatically on each new URI (see
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

Post-EQ clipping is what clip protection actually guards against: the EQ can elevate peaks
above 0 dBFS even with a sane-looking preamp, especially on already-mastered pop/rock material.
When `Clip protection = Off`, the contract explicitly permits clipping and the application must
not pretend it was prevented. `Soft` is opt-in: the shipped default remains `Off` (per the
bounded user-surface table and the fresh-install state in *Persistence*), and the UI may
describe `Soft` as the suggested choice when the user first enables the equalizer, but it never
changes the persisted default.

## Live-reconfiguration boundary

The contract intentionally limits which knobs can change mid-playback and at which pipeline stage
the changes take effect. The boundary is:

| Knob | Mid-playback? | Mechanism |
| --- | --- | --- |
| `Enabled` | yes | Pause → bin install/remove → resume |
| `Preset` | yes | Idle pad-probe buffer-boundary transaction (multi-property) |
| `Preamp` | yes | Direct single-property write (no probe) |
| Single band `bandN` | yes | Direct single-property write (no probe) |
| Multiple bands at once | yes | Idle pad-probe buffer-boundary transaction (multi-property) |
| `Clip protection` | yes | Dynamic in-bin `rglimiter` insert/remove under a blocking pad probe |
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
writes (`g_object_set`) on the running elements inside the bin so that coefficients update
during playback without a pipeline state transition. No re-link, no re-instantiate, no seek, no
EOS-resending. `g_object_set` is atomic per property with respect to the streaming thread — the
element's object lock serializes the write against buffer processing — so a single write takes
effect on the first buffer that element processes after the write returns.

The contract does **not** use `g_object_freeze_notify` / `g_object_thaw_notify` to batch UI
updates, and does not claim cross-element atomicity from them. Freezing a GObject only
defers `notify` emission; on thaw every changed property still emits its own `notify`, so
ten `bandN` writes still produce ten emissions, and the preamp lives on a separate `volume`
GObject that the freeze does not cover at all. The one-update-per-transaction UI guarantee
is delivered by a module-level aggregate notification instead: when the probe callback (or
a single-property write) finishes applying a batch, the equalizer module emits exactly one
settings-changed event carrying the applied `EqSettings` snapshot, and the UI re-renders
from that single snapshot. Per-property `notify` signals remain connected for tests and
diagnostics, but the UI update path is the aggregate event only. The aggregate event
provides no audio-buffer ordering guarantee and makes none; ordering is the probe's job.

The *buffer-boundary transaction* required for multi-property updates (preset loads, multi-band
batched edits) is an **idle pad probe** on the bin's sink ghost pad — the mechanism GStreamer
actually provides for running code between buffers:

1. Capture the new band vector and preamp into a single typed struct (`EqSettings`).
2. `gst_pad_add_probe` on `audio-filter-sink` with `GST_PAD_PROBE_TYPE_IDLE`. When the
   streaming thread next finds the bin boundary idle, the probe callback performs every write
   in the batch (ten `bandN` writes on `equalizer-10bands`, one `volume` write on the preamp
   element) and returns `GST_PAD_PROBE_REMOVE`, uninstalling itself.
3. The batch is thereby serialized against buffer flow at the bin boundary: no buffer crosses
   the bin boundary while the batch is mid-write.

The probe bounds, but does not eliminate, transient mixing of old and new values: a buffer
already past the boundary inside the chain renders with the old coefficients while the batch
lands. The contract accepts exactly one buffer of intermediate gain combination (≈ 21 ms at
the default 1024-sample buffer and 48 kHz) as the defined transient; it is strictly bounded
and inaudible in practice. Single-property writes (one band, or the preamp alone) skip the
probe and write directly: exactly one property changes, so there is no intermediate
combination of gains to bound — every buffer renders with the old value until the write
returns, and with the new value from the first buffer that element processes afterwards.
This is the one update mechanism for a preamp-only or single-band edit; no probe variant
exists for it, and the timing expectations in the acceptance matrix are stated against this
paragraph.

No configuration write produces, waits for, or requires any bus message. `GST_EVENT_CAPS` and
`GST_EVENT_SEGMENT` are pad events serialized with the stream; they are not acknowledgements,
and an equalizer update never generates one. Because `g_object_set` is a GObject state
mutation and produces no GStreamer pipeline event, **no `Buffering` message is produced by
configuration updates.** A `Buffering` message observed on the bus during normal playback
originates from the upstream decoder (network radio underrun, file EOF, decoder flush) and is
unrelated to the equalizer update; the UI must not show a configuration-update spinner based
on a `Buffering` message.

`Clip protection` toggles the `rglimiter` element's *presence* inside the installed bin as a
dynamic topology change while the pipeline stays in `Playing` — an in-bin element
insert/remove requires no pipeline pause. The topology edit must never run on live data
flow: between the unlink and the re-link the EQ output pad has no downstream peer, and a
buffer pushed in that window fails with `GST_FLOW_NOT_LINKED`, which the bus surfaces as a
pipeline error before the fallback path can run. The edit is therefore performed inside a
*blocking pad probe*, the mechanism GStreamer provides for rewiring topology between
buffers:

1. `gst_pad_add_probe` on the `equalizer-10bands` src pad with
   `GST_PAD_PROBE_TYPE_BLOCK_DOWNSTREAM` combined with `GST_PAD_PROBE_TYPE_IDLE`. The probe
   blocks the next buffer (or fires immediately on an idle boundary) and holds data flow
   stopped for as long as its callback runs, so no buffer can reach the pads being rewired.
2. Inside the probe callback: unlink `equalizer-10bands` from its downstream neighbor, add
   or remove `rglimiter` inside the bin (on add: link it, then
   `gst_element_sync_state_with_parent` so the element's state follows the running bin; on
   remove: unlink it, then set it to `NULL` before dropping the reference), and re-link the
   chain (`equalizer-10bands` ↔ `rglimiter` ↔ post-EQ `audioconvert`, or directly
   `equalizer-10bands` ↔ post-EQ `audioconvert` when the limiter is removed).
3. Return `GST_PAD_PROBE_REMOVE` from the callback so the probe uninstalls itself and
   blocked data flow resumes across the new topology.
4. Mark the change in metrics as a brief swap (≤ 100 ms by spec).

Because every step of the topology edit happens while the blocking probe holds the stream,
no buffer ever observes the intermediate unlinked state and `GST_FLOW_NOT_LINKED` cannot
reach the bus from this path.

If the dynamic re-link fails, the implementation falls back to the pause/relink seam (pause
the pipeline, add/remove and link, resume). If the limiter cannot be attached by either path,
the failure is a recoverable error: the chain degrades to the no-limiter layout and the
user-visible status becomes the same as `Clip protection = Off`. When the playing track is a
live stream with no `gapless` table (e.g. a remote radio URL), the same dynamic seam is used,
a one-time metadata-free "reconfiguring audio output" diagnostic is published, and element
insert/remove at the bin boundary does not disturb the upstream decoder's buffering.

## Persistence

Persisted equalizer state is fifteen keys in six logical groups, in a single file. The on-disk
grammar is:

```ini
key="value"
key="value"
...
```

Where:

- Every key is one of the fifteen listed below and appears exactly once, in the listed order
  (the parser is order-insensitive on read but the writer always emits them in the canonical
  order so diffs and bugs are reproducible).
- Every value is a double-quoted UTF-8 string. Quotes inside a value are escaped as `\"`; the
  backslash is escaped as `\\`; newline characters (LF or CR) are not permitted in values.
  Floats are emitted with one decimal place and no explicit `+` sign (e.g. `"-24.0"`, `"0.0"`,
  `"12.0"`) so that precision is bounded by the schema, not by the writer's locale or precision
  settings; a leading `+` is accepted on read as an equivalent value. The parser requires the
  value to be double-quoted; an unquoted value is a malformed file and triggers the validation
  rule below.

The fifteen keys, in six logical groups:

| Key | Type | Range / values |
| --- | --- | --- |
| `schema_version` | quoted integer literal | `"1"` (the only supported value at this revision) |
| `enabled` | quoted boolean literal | `"true"` / `"false"` |
| `preset` | quoted preset name | `"flat"`/`"pop"`/`"rock"`/`"jazz"`/`"classical"`/`"custom"` |
| `preamp_db` | quoted float (one decimal) | `"-24.0"` … `"0.0"` … `"12.0"` |
| `band0_db`…`band9_db` | quoted float (one decimal) | `"-24.0"` … `"0.0"` … `"12.0"` |
| `clip_protect` | quoted enum | `"off"` / `"soft"` |

`equalizer.cfg` lives in the existing `dirs::data_dir()/tributary/` directory beside
`volume`. The file is owned by the equalizer module; no other module reads or writes it.

The writer uses an *atomic replace* protocol. The steps are normative and platform-neutral;
each platform maps them onto its own operations as specified below:

1. Construct the new content in memory.
2. Create a uniquely named temporary file in the destination's directory, with permissions
   restricted to the invoking user (mode `0600` on platforms that expose per-file modes).
3. Write the entire buffer with a write-all loop that keeps writing until every byte is
   accepted or a permanent error surfaces (Rust's `std::io::Write::write_all` provides exactly
   this semantics). A single `write()` may legally complete with a short byte count even for a
   small regular file, and replacing after a short write would publish a truncated file that
   the next load would classify as malformed and discard.
4. Flush the file's contents to stable storage (`sync_all` — `fsync(2)` on Unix,
   `FlushFileBuffers` on Windows) and close the file.
5. Atomically replace `equalizer.cfg` with the temporary file using the platform's
   rename-within-directory operation (per-platform below).
6. On platforms that provide a directory-sync operation (POSIX platforms), `fsync` the
   destination directory so the new directory entry is durable.

The content is bounded by the schema — at most a few hundred bytes even at the maximum band
precision — so the loop terminates in one pass in practice; the loop, not the size assumption,
is what makes the published file complete. On every supported platform, an on-disk reader
observes either the prior file or the complete new file, never a partial one. Durability
claims are exactly the ones each platform can make, no more:

- **Unix (Linux, macOS).** The replace in step 5 is `rename(2)` within the destination
  directory, which is atomic with respect to readers. After the file sync in step 4 and the
  directory sync in step 6 return, the new content is durable across a power loss. On
  filesystems that do not honor a directory `fsync` (some macOS configurations), the replace
  remains atomic for readers, and durability of the newest directory entry degrades to what
  the filesystem itself guarantees; the contract does not claim more than the platform
  delivers.
- **Windows.** The replace in step 5 is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` and
  `MOVEFILE_WRITE_THROUGH` — the operation `tempfile::NamedTempFile::persist` performs on
  that platform, the same persist-after-sync pattern the repository already uses for XSPF
  export and preference writes. NTFS metadata journaling makes the replace atomic with
  respect to readers and crash-consistent; the file contents were flushed in step 4, and
  write-through covers the replacement itself. Windows has no directory-handle sync
  operation, so step 6 does not apply and no directory-entry durability is claimed beyond
  the write-through semantics of the replace call.

The temp name is minted fresh for every write attempt — a random segment (for example a UUID, as
in the existing XSPF-export writer) beside the destination — and created exclusively (a
no-overwrite create: `O_EXCL` on Unix, `CREATE_NEW` on Windows, as `tempfile::NamedTempFile`
provides). A fixed sibling name such as `equalizer.cfg.tmp` is
explicitly rejected: exclusive creation with a fixed name does not serialize writers, it makes
the second writer's create fail with `AlreadyExists`, and a crash between create and replace
would wedge every later save behind a stale temp the next writer must not delete. With a unique
name, every writer owns a private staging path, so interleaved or crash-interrupted writes can
never corrupt each other's temp file; the replace operation itself is atomic with respect to
the destination on every supported platform and publishes that writer's complete content or
nothing. Within the application the equalizer module is the sole writer — the debounced save
and the shutdown flush both run on the GTK main loop — so completed replaces are ordered and
last-writer-wins by construction.

Recovery edges are specified, not incidental: if exclusive creation reports `AlreadyExists`
(name collision, vanishingly unlikely with a random segment), the writer mints a new unique
name and retries rather than deleting the existing file. If any step fails before the replace,
the writer removes only the temp file it created for that attempt and leaves the destination
untouched. A temp file orphaned by a crash between creation and replace is inert garbage:
readers only ever open `equalizer.cfg` itself, and no code path deletes a temp whose name it
did not mint, so no recovery sweep is required or permitted.

Persistence uses a debounced single-writer pattern: a 750 ms idle interval coalesces slider-drag
changes into one write per change-spell, and the save runs on the GTK main loop. Every
change-spell writes, including one whose result is exactly the fresh-install default state
(Enabled `false`, preset `Flat`, all bands zero, preamp zero, clip protection `Off`). There is
no default-state suppression: suppressing that write would leave an older non-default file on
disk, and the next application start would resurrect settings the user visibly reset (for
example, a user moving `Clip protection` from `Soft` back to `Off` must see `Off` after a
restart). The debounce timer is reset on every change so drag-induced writes are flushed on the
trailing edge of the gesture.

In addition to the debounce, the equalizer module registers a *shutdown flush* with the
application's single quit seam: the window close-request drain (`connect_close_request`), the
same handler this application already uses for module shutdown work. Both quit entry points
route through it — a direct window close, and the in-app quit request, which dispatches
`window.close()` from the application's quit handler with an `Application::quit` fallback only
when no window exists — so the flush runs exactly once, inside the drain, before the GTK main
loop exits. The flush synchronously performs an atomic-replace write of the current state to
disk. It is its own uniquely named write-temp-and-replace cycle; it does not wait for the
debounce timer and runs even if the timer is armed. This guarantees that no partial write is
observable on disk if the user quits while the debounce is pending.

The no-window `Application::quit` fallback requires no separate flush: the settings panel lives
in the single window, so once that window's close-request drain has run, no equalizer state
change can occur before process exit. Out-of-band process termination (`SIGTERM`, `SIGINT`,
`SIGKILL`) is not hooked — the application installs no main-loop signal handlers, and this
contract adds none. On such termination the flush does not run, and the durability guarantee
degrades to the debounce bound: at most one 750 ms change-spell since the last completed write
is lost, and no partial write is observable on disk, which is the atomic-replace protocol's own
guarantee, independent of the flush.

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
- A file that parses but omits any of the fifteen keys is malformed: it is replaced with the
  defaults as a whole under the rule below. There are no per-key defaults for missing keys —
  partial files are not merged with defaults, because silently filling gaps would combine
  stale band values with fresh ones.
- A file that contains any key more than once is malformed: it is replaced with the defaults
  as a whole, and the diagnostic names the duplicated key. No precedence between duplicate
  lines is defined — the parser is order-insensitive, so a positional rule (first or last
  wins) could not be stated without making the parsed state depend on line order; a duplicate
  key indicates a corrupted or hand-edited file, and the canonical writer emits exactly one
  line per key.
- `bandN_db` values outside `[-24.0, +12.0]` are clamped to the boundary.
- `preamp_db` outside `[-24.0, +12.0]` is clamped to the boundary.
- A `preamp_db` or `bandN_db` value inside the range but not an exact multiple of 0.5 (for
  example `0.1` or `3.7`) is snapped to the nearest 0.5 dB step, ties away from zero, before
  use. The snapped value is what the runtime materializes and what the next save persists, so
  no off-grid value crosses the read boundary into the runtime — the bounded user surface
  would reject it, and runtime code must not be able to materialize it either. Off-grid
  values are coerced per key exactly like the range clamps above; they do not invalidate the
  file.
- `preset` outside the named set (including unknown legacy values) is first coerced to
  `"flat"`; the band vector is *not* reset by this coercion, only the persisted name is
  rewritten, and the combined result is then subject to the preset-truth reconciliation below.
- `enabled` not parseable as bool becomes `"false"`.
- `clip_protect` outside `"off"` / `"soft"` becomes `"off"`.
- *Preset-truth reconciliation.* After the per-key coercions above, if the persisted `preset`
  names one of the five canonical presets, the coerced band vector and preamp are compared
  against that preset's canonical definition (the appendix): an exact match keeps the name,
  and any difference moves the persisted preset to `"custom"`. Canonical values are never
  substituted for stored values — the coerced values are what the runtime applies and what
  the next save persists. For example, `preset="pop"` with `band0_db="3.7"` snaps to 3.5 dB,
  which is not Pop's canonical +1.0 dB, so the persisted preset becomes `"custom"` and the UI
  displays `Custom`. The same reconciliation covers a named preset whose preamp was clamped
  or snapped away from its canonical recommended value. A preset coerced from an unknown
  name to `"flat"` survives as `Flat` only if the resulting vector and preamp match canonical
  Flat (all zeros); otherwise it too becomes `"custom"`. This reconciliation is a
  name-side transition only: it never alters any stored or coerced value.

A malformed file is replaced with the default state via the same atomic-replace protocol above,
the user's prior preferences are recorded in a typed diagnostic with file path, byte count, and
the bad key, and the change is not silent.

A *transient read failure* is not a malformed file. If `equalizer.cfg` exists but cannot be
opened or read (for example `EACCES`, `EBUSY`, or `EIO`), the module runs with in-memory
defaults for the session and publishes the same warn-level diagnostic — and it does **not**
schedule any write. The debounced writer and the shutdown flush are both suppressed until a
subsequent read of the file succeeds and reconciles the in-memory state with disk. A transient
unreadable file must never cause a valid on-disk file to be overwritten with defaults: the
defaults-overwrite path above is reserved for files whose *content* is malformed, which is
established only by a read that succeeds.

## Capability matrix

The capability matrix below assigns a fixed status to each output for equalizer DSP. The status
is communicated to the UI through a new trait method, and the rendering is fixed per status; a
shipped output cannot claim an unsupported status, and a future implementation that adds a
status must ship the matching UI rendering at the same time.

| Output | Equalizer DSP |
| --- | --- |
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
- MPD: The MPD protocol exposes no native equalizer command (`setvol` controls playback volume,
  not equalization), and server-side equalization would require editing MPD's own filter
  configuration on the server host, which the protocol does not let a client do. The canonical
  contracted behavior is host-side rendering, so host EQ does not reach the receiver.

For each `unsupported` output, the user-visible settings UI renders the equalizer controls as
disabled, and the limitation is explained by visible text rendered inside the panel (e.g.
"AirPlay receivers render audio end-to-end, so Tributary's equalizer cannot reach the
speaker."). The same sentence is additionally attached to the disabled controls as their
accessible description (the toolkit's `aria-describedby`-equivalent relation). A tool-tip alone
is not sufficient: disabled controls are frequently not focusable, so a tooltip-only
explanation makes the limitation undiscoverable by keyboard and screen-reader users. Disabled
controls preserve the last-saved values locally.

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
- **Reload defaults from disk.** Forces the EQ module to re-read `equalizer.cfg` immediately,
  applying the same validation rules as on load — including the automatic atomic-replace repair
  of a malformed file, which is therefore not something the user must remove by hand. The
  reload is synchronous and does not wait for a change-spell; the debounced single-writer still
  applies on the next change-spell.

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
- The `equalizer.cfg` file path is logged at debug only in normal operation, with exactly two
  deliberate exceptions: the malformed-file diagnostic and the transient-read-failure
  diagnostic publish the full path at warn, because the path is the remediation target the
  user must inspect or fix. Outside those two warnings, no log at info or above carries the
  path, and the informational chain insert/remove messages never carry it.
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
migration of older non-English keys is not expected; an unknown preset value is coerced to
`flat` and then follows the same validation rules as any other value, including the
preset-truth reconciliation in *Persistence*.

When the active output does not support the equalizer, the panel's explanation of that
limitation is rendered as visible, localizable text associated with the disabled controls via
the toolkit's accessible-description relation, as specified in *Capability matrix*; the
explanation is never tool-tip-only.

## Acceptance matrix

Implementation acceptance requires the exact conditions listed below. The matrix is exhaustive
for this contract; new conditions require a new revision.

1. **Fresh install, EQ disabled.** Persisted state matches the default; `playbin3.audio-filter`
   is `NULL`; no equalizer bin is constructed.
2. **Enable EQ on local output.** Equalizer bin is constructed and installed at
   `playbin3.audio-filter` via the pause/relink seam; the pre-EQ `capsfilter` pins
   `audio/x-raw, format=F32LE, channels=2, layout=interleaved` (rate negotiated across the
   ghost pads); pipeline returns to `Playing`; total swap ≤ 100 ms.
3. **Change a single band mid-playback.** Single-property write reaches `equalizer-10bands`;
   buffer passes; no gapless discontinuity; new value reaches the filter on the next buffer.
4. **Select Pop preset mid-playback.** `EqSettings` struct captures ten bands + preamp; one
   idle-pad-probe transaction on `audio-filter-sink` writes all ten bands and the preamp
   between buffers; the UI observes one update per transaction via the aggregate
   settings-changed event; preset combo displays `Pop`.
5. **Manual band edit mid-playback.** Single property write on `bandN`; persisted `preset`
   field becomes `custom`; UI combo displays `Custom`.
6. **Cycle clip protection Off → Soft → Off.** Dynamic in-bin insert/remove under a blocking
    pad probe, with state sync each time, so no buffer observes the intermediate topology; no
    pipeline pause; total swap ≤ 100 ms per toggle; pause/relink fallback exercised if the
    dynamic re-link is forced to fail.
7. **Sine input above +6 dBFS with clip protection = Soft.** Output peak converges
   asymptotically to 0 dBFS without exceeding it; soft-knee compression engages at the −6 dBFS
   threshold; a 0 dBFS input is attenuated by approximately 1.1 dB; reflects the `rglimiter`
   element's own description "Apply signal compression to raw audio data".
8. **Switch active output to AirPlay.** EQ module renders disabled in UI;
   `playbin3.audio-filter` becomes `NULL`; no equalizer DSP runs.
9. **Switch active output back to Local.** EQ module renders enabled in UI if previously
   enabled; bin re-installs at `playbin3.audio-filter` with persisted bands and preamp.
10. **Quit while a slider drag is in progress.** The quit request reaches the window
    close-request drain (a direct window close and the in-app quit request both route through
    `window.close()`); the shutdown flush writes the current state to disk via atomic replace
    before the drain completes; no partial writes.
11. **Malformed `equalizer.cfg` on disk.** Defaults re-written via atomic replace; single
    warn-level diagnostic published (file path, byte count, bad key).
12. **Preamp outside bounds in saved file.** Value clamped to range; bands remain as stored;
    the preset-truth reconciliation applies — a named preset whose clamped preamp differs from
    that preset's canonical recommended preamp becomes `custom`.
13. **Band value outside bounds in saved file.** Value clamped to range; other bands remain
    as stored; the preset-truth reconciliation applies — a named preset whose clamped vector
    no longer matches its canonical definition becomes `custom`.
14. **Preset name not in the named set on disk.** Coerced to `flat`; band vector remains as
    written on disk; the preset-truth reconciliation then keeps `flat` only when the vector
    and preamp match canonical Flat, and otherwise moves the persisted preset to `custom`.
15. **Hardware sink with 8-channel layout (macOS).** Pre-EQ `capsfilter` caps remain
    `channels=2`; `audioconvert` performs the downmix conversion work; EQ runs in stereo; same
    cap fix as existing module.
16. **Gapless album transition.** Bin persists across URI transitions; equalizer state is not
    re-applied automatically on each new URI; no audible discontinuity.
17. **Preamp `0.0 dB` selected.** Preamp `volume` element is in the chain with `volume=1.0`;
    gain flat.
18. **Transient unreadable `equalizer.cfg` (open/read error, file exists).** No write is
    scheduled; the on-disk file is untouched; warn diagnostic published; the session runs with
    in-memory defaults; a later successful read reconciles state with disk.
19. **Unrelated playback error with EQ installed (decoder/network/sink failure).** The
    equalizer bin remains installed; equalizer state is untouched; the passthrough fallback is
    not triggered by a non-equalizer failure.
20. **Off-grid persisted gain in saved file (e.g. `band0_db="3.7"`).** Value snapped to the
    nearest 0.5 dB (ties away from zero); other keys remain valid; the snapped value is what
    the runtime applies and what the next save persists. When the persisted preset is a named
    one, the preset-truth reconciliation applies (condition 21).
21. **Named preset with off-grid persisted gain (`preset="pop"`, `band0_db="3.7"`).** The band
    snaps to 3.5 dB; Pop's canonical band 0 is +1.0 dB, so the coerced vector does not match
    the appendix definition; the persisted preset becomes `custom`; the UI combo displays
    `Custom`; the runtime applies the coerced values, and the next save persists `custom`
    with them (no canonical value is substituted).
22. **Preamp-only edit mid-playback.** Direct single-property write on the `volume` element
    with no pad probe; the new gain applies from the first buffer that element processes
    after the write returns; no multi-value transient is defined or observed; the UI observes
    one aggregate settings-changed event; a manual edit moves the persisted preset to
    `custom` per *Band and preamp mechanics*.
23. **Save on any supported platform, including Windows.** The write completes through the
    platform's atomic-replace path (Unix: `rename(2)` after file and directory sync;
    Windows: `NamedTempFile::persist` with write-through); a concurrent or subsequent reader
    observes either the prior file or the complete new file, never a partial one; the
    durability claim is the platform's own — file contents are flushed before the replace,
    and directory-entry durability is claimed only on platforms that provide a directory
    sync.
24. **Duplicate key in saved file (e.g. a second `band0_db` line).** The file is malformed as
    a whole; defaults are re-written via atomic replace; the warn diagnostic names the
    duplicated key; no precedence between the duplicate lines is defined or observed.

## Implementation boundary

The [implementation record](task.md#p24--audio-processing-and-output-protocols) in `task.md` P2.4
lands this contract. The record is
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
  audioconvert ! audioresample`) as specified in *Filter graph* above.
- Adds the `rglimiter` insert/remove inside the bin behind the single `Clip protection`
  boolean, as a dynamic state-synced topology change with the pause/relink fallback.
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
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Flat | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| Pop | −2.0 | +1.0 | +2.0 | +3.0 | +2.0 | 0.0 | −1.0 | −1.0 | 0.0 | +1.0 | +2.0 |
| Rock | −1.0 | +3.0 | +2.0 | 0.0 | −1.0 | −1.0 | +0.0 | +2.0 | +3.0 | +3.0 | +2.0 |
| Jazz | −1.0 | +2.0 | +1.0 | 0.0 | +1.0 | +1.0 | +0.0 | +1.0 | +2.0 | +2.0 | +1.0 |
| Classical | −2.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | +1.0 | +2.0 | +3.0 |

Column headers above are the band centre frequencies in Hz. All values are linear dB and rounded
to one decimal. Any deviation from this table is a contract change and must be reflected in the
appendix before the matching code lands.
