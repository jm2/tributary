# OwnTone AB1/AB2 corrective map

Corrective report: `refinery-20260916-6233a611-tr-t3a/corrective-instructions.md`.
Rejected head: `6233a61164d24db918818240af005887c0b9e72c`.

## AB1 — supported configuration and effective scanned input

The invented `pipe_path` directive is removed. `config_binds_pipe` now delegates
to `airplay_owntone_config::binds_pipe`, which reads sections, assignments, quoted
strings and lists. Only the effective `library.directories` list binds input;
matching text in another section or a comment cannot supply the binding.
The single absolute directory must canonically equal the configured FIFO parent.
The input must have a non-hidden `.pcm` name and cannot itself be a symlink.
Scanning and pipe autostart must be enabled, PCM must be 44100 Hz/16 bit, and
nonempty ignore lists are refused. Omitted settings use the pinned defaults.

This intentionally accepts a documented dedicated-instance subset, not every
libconfuse configuration. Duplicate sections/options, includes, expansion,
escapes, titled sections and unsupported library options fail closed. The
existing effective `-c` option, canonical config file, executable/endpoint process
identity, ownership record and exclusive instance lock remain in force.

Source contracts are OwnTone 29.3 commit
`d6fb3edf5831de38134ebd92fcf09a730ddd37aa`:

- [Configuration schema](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/conffile.c):
  `sec_library` defines directories, scan filters, autostart and PCM defaults.
- [Scanner](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/library/filescanner.c):
  file classification excludes hidden/control/ignored inputs.
- [Configuration example](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/owntone.conf.in):
  `pipe_autostart` watches named pipes in the scanned library for incoming data.

Hermetic daemon configurations now come from
`tests/fixtures/owntone-29.3-library.conf`, including the stalled-endpoint fixture.
An independent check compiled the **exact pinned upstream `sec_library`** with
installed libconfuse 3.4: the fixture and a directories-only configuration using
upstream defaults parse successfully; the former `pipe_path` configuration fails.
No OwnTone daemon or physical receiver was run for that schema check.

Regression coverage:

- `pinned_library_config_binds_only_an_enabled_unfiltered_pcm_input` accepts the
  pinned fixture/defaults and rejects wrong sections/directories, relative paths,
  disabled autostart/scans, wrong PCM format, ignore filters, duplicate options,
  includes, comments-only bindings, hidden/control filenames and FIFO symlinks.
- `quoted_hash_is_not_a_comment_and_ambiguous_syntax_is_refused` covers quoted
  comment characters and incomplete/unsupported syntax.
- Existing `cmdline_binding_requires_the_effective_configuration_and_pipe`
  assertions remain, now using supported library configuration.

## AB2 — volume query and strict HTTP contract

`OwnToneClient::set_volume` URL-encodes `volume` as a query parameter and sends
`PUT /api/player/volume?volume=<percent>` without a JSON body. It retains the
existing bounded request, HTTP-failure propagation and initial-volume ordering.
This matches the pinned
[`jsonapi_reply_player_volume` handler](https://github.com/owntone/owntone-server/blob/d6fb3edf5831de38134ebd92fcf09a730ddd37aa/src/httpd_jsonapi.c#L2874).

The recording daemon validates the method and numeric volume query (0–100),
rejects missing/malformed/body-only volume requests with HTTP 400, and rejects
unknown endpoints instead of treating every PUT as successful.

- `volume_contract_rejects_body_only_and_propagates_http_failure` proves the old
  body-only request and invalid values fail and valid boundary values succeed.
- `the_initial_volume_is_applied_before_the_first_play_through_open` retains its
  real-open/order/cleanup assertions, checks `0.42 -> volume=42`, and exercises
  the live session control with `0.73 -> volume=73` against that strict daemon.
- Previous event, cancellation, terminal-cache, instance-lock and route-custody
  assertions are preserved.

## Scope

These corrections resolve the two specific upstream compatibility defects.
They do not certify the wider U5 runtime/authority scope, scan completion, actual
PCM consumption by OwnTone, or real-device playback. Physical validation remains
separate. The refinery must independently review the new head.
