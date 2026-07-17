# Audio test fixtures

## `silence.flac`

This is a deterministic 100 ms FLAC containing 800 mono, signed 16-bit zero samples at 8 kHz.
It is intentionally tiny (99 bytes) and has no initial user tags, padding, or seek table.

The fixture was generated with `flac 1.5.0` (reference libFLAC 1.5.0, 2025-02-11):

```sh
truncate -s 1600 /tmp/tributary-silence.raw
flac --force --force-raw-format --sign=signed --endian=little \
  --channels=1 --bps=16 --sample-rate=8000 --no-padding --no-seektable \
  --compression-level-8 --output-name=tests/fixtures/audio/silence.flac \
  /tmp/tributary-silence.raw
```

SHA-256:

```text
c47ed5dbe255701328f28b58fbe7408a70ae2ad20057089b5393253a00eab946  silence.flac
```

The PCM source is mechanically generated silence and contains no third-party recording. The
fixture is distributed under Tributary's GPL-3.0-or-later license.
