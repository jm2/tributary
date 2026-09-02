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

## `flac_date_2007.flac` and `ogg_date_2007.ogg`

These are 200 ms FLAC / Ogg-Vorbis files of 8 kHz mono silence tagged with the
Xiph-standard `DATE=2007` field (the conventional year field for these
formats), plus text fields carrying trailing spaces and/or leading space:

| Field    | Value                      | Import assertion           |
|----------|----------------------------|----------------------------|
| `date`   | `2007`                     | year recognized            |
| `title`  | `Two  Spaces  Trailing  `  | internal spaces kept, trailing trimmed |
| `artist` | `  Flac Artist  ` / `  Ogg Artist  ` | leading space kept, trailing trimmed |
| `album`  | `Pad Album  `              | trailing trimmed           |
| `genre`  | `Rock  `                   | trailing trimmed           |
| `composer` / `album_artist` | `Pad Composer  ` / `Pad AlbumArtist  ` (FLAC only) | trailing trimmed |

They were generated with `ffmpeg 6.1` from mechanically generated silence
(deterministic zero samples, no third-party recording):

```sh
# FLAC
ffmpeg -f lavfi -i anullsrc=r=8000:cl=mono -t 0.2 -c:a flac \
  -metadata date=2007 \
  -metadata title='Two  Spaces  Trailing  ' \
  -metadata artist='  Flac Artist  ' \
  -metadata album='Pad Album  ' \
  -metadata genre='Rock  ' \
  -metadata composer='Pad Composer  ' \
  -metadata album_artist='Pad AlbumArtist  ' \
  tests/fixtures/audio/flac_date_2007.flac
# Ogg Vorbis
ffmpeg -f lavfi -i anullsrc=r=8000:cl=mono -t 0.2 -c:a libvorbis \
  -metadata date=2007 \
  -metadata title='Two  Spaces  Trailing  ' \
  -metadata artist='  Ogg Artist  ' \
  tests/fixtures/audio/ogg_date_2007.ogg
```

SHA-256:

```text
0c273980f9aed559f52419465f8cb850d794b65f8b4b8c75720b803946733868  flac_date_2007.flac
a190d69b6a6cd7e8abd2b3c8ae926df9ad7772d73dc47d2c7e69ed89af409076  ogg_date_2007.ogg
```

## `id3v1_padded.mp3`

This is a 200 ms MP3 of 8 kHz mono silence with a hand-built legacy ID3v1 tag
whose fixed-width fields are padded with spaces (0x20) instead of NUL bytes —
the classic layout produced by ancient taggers, and the source of the padded
metadata complaint. Field contents: `title` = `Pad Title`, `artist` =
`Pad Artist`, `album` = `Pad Album`, `year` = `2007`, `comment` =
`Pad Comment`, `genre` byte = `0xFF` (unset). The audio payload is a plain
MPEG layer III frame stream; the 128-byte `TAG` block was appended verbatim:

```sh
ffmpeg -f lavfi -i anullsrc=r=8000:cl=mono -t 0.2 -c:a libmp3lame \
  -metadata title=carrier carrier.mp3
# Strip the leading ID3v2 block (synchsafe size at bytes 6..9), then append:
# 'TAG' + 30B title + 30B artist + 30B album + 4B year + 30B comment + 1B genre,
# every 30-byte field space-padded to its full width with no NUL terminator.
```

SHA-256:

```text
081f654efe23c32555b2a223d2dacbf339227d8461a7465d7bdd24cc187325ed  id3v1_padded.mp3
```

The audio payloads are mechanically generated silence and contain no
third-party recording. The fixtures are distributed under Tributary's
GPL-3.0-or-later license.
