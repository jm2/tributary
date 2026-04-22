# Changelog

All notable changes to Tributary are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1] — Unreleased

### Changed
- **Modularized `window.rs` (3,730 → 1,776 lines)** — Refactored the monolithic main window module into 6 focused sub-modules for improved maintainability and AI-assisted development:
  - `window_state.rs` — Shared `WindowState` struct bundling 16+ `Rc<RefCell<…>>` UI state fields for dependency injection across modules.
  - `playlist_actions.rs` — Playlist CRUD logic (create, rename, delete, reorder entries).
  - `output_switch.rs` — Output selector popover click handler (local, MPD, AirPlay, Chromecast switching).
  - `context_menu.rs` — Tracklist right-click menu (add/remove from playlist, Properties dialog).
  - `discovery_handler.rs` — mDNS/DNS-SD event handler (sidebar + output list add/remove for discovered servers, AirPlay receivers, and Chromecast devices). Deduplicated device-check logic into shared helpers.
  - `source_connect.rs` — Sidebar selection-changed handler (source switching for local, playlist, USB, radio, connected remote, and unauthenticated remote sources with auth dialog flows).
- **Dead code cleanup** — Removed orphaned `disable_popover_scrollbars` from `window.rs` (now only in `context_menu.rs`). Cleaned up unused imports (`sea_orm`, `AirPlayOutput`, `ChromecastOutput`, `MpdOutput`, radio functions, `show_auth_dialog`) that migrated to sub-modules.

## [0.4.0] — 2026-04-19

### Added
- **Chromecast audio output** — New `src/audio/chromecast_output.rs` implementing the `AudioOutput` trait for Chromecast (Cast V2) devices using the MIT-licensed `rust_cast` crate. `OutputType::Chromecast` variant added to the output type enum. Discovery integration browses `_googlecast._tcp.local.` via mDNS and automatically adds discovered Chromecast devices to the output selector popover with `"video-display-symbolic"` icon. The friendly name is extracted from the mDNS `fn` TXT record field. Clicking a Chromecast row connects via TLS (port 8009), launches the Default Media Receiver app, and sends `media.load()` with the track URL. Supports play, pause, stop, seek, and volume control (0.0–1.0 linear). Remote sources (Subsonic, Jellyfin, Plex, radio) work out of the box since their stream URLs are already HTTP; local `file:///` URIs are rejected with a clear error message (embedded HTTP server for local file casting deferred to a follow-up). Includes 7 unit tests.
- **`-Test` flag for `build-windows.ps1`** — New quick-exit mode that sets up the MSYS2 build environment and runs `cargo test --release --target $RustTarget`. Useful for running the test suite from PowerShell without a full DLL-bundled build.
- **`-Run` flag for `build-windows.ps1`** — New quick-exit mode that builds in release mode and launches the compiled binary directly from PowerShell. Useful for quick iteration without manual DLL bundling.
- **Audio output abstraction** — New `AudioOutput` trait (`src/audio/output.rs`) that abstracts playback destinations (local speakers, MPD, future AirPlay). `LocalOutput` wraps the existing GStreamer `playbin3` pipeline. `MpdOutput` sends MPD protocol commands over TCP to remote or local MPD servers with newline-sanitised command injection prevention and 5-second connection timeouts. Designed for future AirPlay 2 (shairport-sync) extension.
- **MPD output backend** — `src/audio/mpd_output.rs` implements the `AudioOutput` trait for Music Player Daemon servers. Commands are sent on background threads to avoid blocking the GTK main thread. Includes `probe()` for TCP connectivity validation and 5 unit tests for the newline sanitisation security layer.
- **Output selector UI (iTunes AirPlay-style)** — New `MenuButton` with a `Popover` in the header bar (between volume slider and hamburger menu) showing available audio output destinations. "My Computer" is always present; MPD outputs are added via a "+" button and persisted to `outputs.json`. The output selector follows the iTunes/Apple Music AirPlay popover pattern with icon + name + checkmark rows.
- **Output switching wired** — Selecting an output in the popover now routes all playback commands (play, pause, stop, seek, volume) through the `AudioOutput` trait abstraction. `PlaybackContext` uses `Rc<RefCell<Box<dyn AudioOutput>>>` instead of a direct `Player` reference. The volume slider disables when `supports_volume()` returns false (e.g., MPD). The `Player::event_sender()` method allows non-GStreamer outputs to feed events into the same `player_rx` event loop.
- **Add Output dialog** — Clicking the "+" button in the output selector popover opens an "Add Output" dialog for MPD servers with Name, Host, and Port fields. The dialog probes the MPD server on a background thread (5-second TCP timeout) before saving, validating the `OK MPD x.y.z` greeting line.
- **Output selector row-click switching** — Clicking a row in the output selector popover now swaps the active audio output. Selecting "My Computer" (index 0) restores the local GStreamer output; selecting an MPD output constructs a new `MpdOutput` and routes all playback through it. The local output is parked (not destroyed) when switching to MPD and restored when switching back. Checkmark visibility updates to show the active output, the volume slider disables for MPD (which manages its own volume), and the popover auto-closes after selection.
- **Haversine geo-distance module** — New `src/radio/geo.rs` with a Haversine great-circle distance function, US state centroid lookup table (50 states + DC), and country centroid lookup table (~60 countries). All data is compiled-in with no external API dependency. Enables client-side distance estimation for radio stations that have country/state metadata but lack geographic coordinates. Includes 11 unit tests covering same-point, NYC→London, antipodal, symmetry, case-insensitive lookups, and interstate distance calculations.
- **Geo-distance sorting for Stations Near Me** — The "Stations Near Me" results are now sorted by estimated distance from the user using the Haversine formula. Stations with actual `geo_lat`/`geo_long` coordinates get exact distances; stations with only a US state name use state centroid lookups; country-only stations use country centroid lookups. This produces a much more useful ordering where nearby stations appear first regardless of which tier they came from.
- **AirPlay output scaffolding** — New `src/audio/airplay_output.rs` implementing the `AudioOutput` trait for AirPlay (RAOP) receivers. `OutputType::AirPlay` variant added to the output type enum. Discovery integration browses `_raop._tcp.local.` via mDNS and automatically adds discovered AirPlay receivers to the output selector popover (no manual "+" button needed). Streaming implementation is scaffolding-only — the GStreamer `raopsink` and `shairport-sync` pipe backends are stubbed with fallback error reporting. Includes 6 unit tests.
- **USB device scaffolding** — New `src/device/mod.rs` with `Device` trait, `DeviceInfo` struct, and `DeviceType` enum for portable music device abstraction. `src/device/usb.rs` implements platform-specific USB mass storage detection: Linux scans `/media/$USER/` and `/run/media/$USER/`, macOS scans `/Volumes/` (excluding system volumes), Windows enumerates drive letters via `GetDriveTypeW`. Includes 6 unit tests. (#1, #8)
- **AirPlay output wired** — Clicking a discovered AirPlay receiver in the output selector popover now constructs an `AirPlayOutput` and routes all playback through it. The GStreamer `raopsink` pipeline (`uridecodebin ! audioconvert ! avenc_alac ! raopsink`) is attempted first; if `raopsink` is unavailable, falls back to `shairport-sync` in pipe mode (Unix only). Host:port is stored on the ListBoxRow widget name during mDNS discovery for precise retrieval on click. Volume slider disables when AirPlay is active. (#7)
- **USB device sidebar integration** — Detected USB mass storage devices now appear under a "Devices" category header in the sidebar on startup. Clicking a USB device scans the volume for audio files using the `tag_parser` and `walkdir` on a background thread, then populates the tracklist with full metadata (title, artist, album, genre, year, bitrate, etc.). The browser panes filter USB device tracks the same way as local library tracks. Scanned results are cached so re-selecting a device is instant. (#1)
- **Album artist sort** — New `album_artist_name` field throughout the data pipeline: DB migration adds the column to the `tracks` table, `lofty` tag parser extracts the `AlbumArtist` tag, all backends (local, Subsonic, Jellyfin, Plex, DAAP) propagate it through to the UI. The browser `TrackSnapshot` includes a `browser_artist()` helper that returns album artist when enabled, falling back to track artist. A `group_by_album_artist` boolean preference is added to `config.json` (default: off). The smart playlist rule engine supports `AlbumArtist` as a filterable text field. (#5)
- **Smart playlist compound sort** — New `sort_order` field on `SmartRules` enables multi-key compound sorting of playlist results. Each `SortCriterion` specifies a field (Artist, AlbumArtist, Album, Title, Year, TrackNumber, DiscNumber, Genre, Duration, Bitrate, PlayCount, DateAdded, DateModified) and direction (Ascending/Descending). Criteria are applied in sequence — the first is the primary sort, subsequent criteria break ties. This enables Tauon-style generator code ordering like "Artist alphabetised → albums in chronological order → track number". The compound sort runs before the existing limit truncation. Fully backward-compatible: `sort_order` defaults to empty via `#[serde(default)]` so existing playlists are unaffected. (#21)
- **Keyboard shortcut: Ctrl+F / Cmd+F to focus search** — Pressing `Ctrl+F` (or `Cmd+F` on macOS) now focuses the browser search entry and makes the browser visible if hidden. (#3)
- **Connection guard for all network sources** — Clicking a sidebar server that is already connecting (spinner visible) no longer opens duplicate auth dialogs or spawns duplicate connection tasks. The guard applies to all backends (Subsonic, Jellyfin, Plex, DAAP) and prevents race conditions when impatient users double-click slow servers. (#13)
- **i18n/l10n framework with 13 languages** — All user-facing UI strings are now wrapped in `rust-i18n`'s `t!()` macro with compile-time YAML translation files in the `locales/` directory. The system locale is auto-detected via `sys-locale` at startup. English is the fallback language. Complete translations included for: Dutch (nl), French (fr), Spanish (es), German (de), Italian (it), Brazilian Portuguese (pt-BR), Polish (pl), Russian (ru), Japanese (ja), Korean (ko), Chinese Simplified (zh-CN), and Chinese Traditional (zh-TW) — covering GNOME Tier 1 language coverage. Column titles and config keys remain in English for persistence compatibility. (#20)

### Fixed
- **Windows i18n not loading (raw translation keys displayed in UI)** — `sys_locale::get_locale()` returns `en-US` on Windows, which doesn't match the `en.yml` locale file. Added locale normalisation that tries the full locale first (preserving `zh-CN`, `zh-TW`, `pt-BR` region-specific files) then falls back to the base language code (`en-US` → `en`). Underscore separators are unified to hyphens for cross-platform consistency (`zh_CN` → `zh-CN`). This also fixes sidebar source ordering, which was broken because untranslated header keys sorted alphabetically instead of in the intended display order.
- **`test_indiana_to_new_york_state` failing in CI** — Widened the assertion range from 800–1000 km to 800–1100 km to match the actual geographic centroid distance of ~1043 km (the New York centroid is in the Adirondacks, not NYC).
- **Non-English locale music folder not scanned** — The default library path now uses `dirs::audio_dir()` (XDG `$XDG_MUSIC_DIR`) which respects the system locale (e.g. `~/Musique` on French systems, `~/Musik` on German). Falls back to `~/Music` only if `audio_dir()` returns `None`. The library engine also reads the configured path from Preferences instead of hardcoding `~/Music`. (#22)
- **Genre displays as blank for tracks without genre metadata** — Tracks with no genre tag now display "Unknown" instead of an empty cell in the tracklist, matching Rhythmbox behaviour. (#10)
- **Last column not resizable** — The rightmost visible column in the tracklist auto-expanded to fill remaining space (a GTK4 `ColumnView` quirk), preventing the user from resizing it by dragging its right edge. Added an invisible zero-width sentinel column that absorbs the auto-expansion, so every real column now has a draggable resize handle. (#12)
- **macOS M4A album art not displaying on some Apple-encoded files** — Replaced the brute-force `covr` atom scanner with a proper recursive MP4 atom walker that follows the standard `moov → udta → meta → ilst → covr → data` path. Now correctly handles the `meta` full-box version/flags prefix, extended 64-bit atom sizes, and deeply-nested atom layouts. The brute-force scan is retained as a fallback for non-standard files.
- **macOS network discovery not retrying after Local Network permission grant** — On macOS, the first launch triggers a Local Network permission prompt while the mDNS daemon is already browsing. After the user grants permission, the daemon doesn't automatically re-browse. Added periodic re-browse logic (every 30 seconds, up to 3 attempts) that fires only when no servers have been discovered yet, so services are found without requiring an app restart.

### Changed
- **Version bumped to 0.4.0** for the new feature release cycle.

## [0.3.1] — Unreleased (development)

### Added
- **Pedantic Clippy analysis** — Enabled `clippy::pedantic` and `clippy::nursery` crate-wide with curated `#![allow(...)]` for GTK-specific patterns. All 392 initial warnings resolved. Pedantic linting now runs in CI on every push/PR and locally via `build-windows.ps1 -Clippy`, `build-linux.sh --clippy`, and `build-macos.sh --clippy`.
- **`cargo audit` in CI** — New Security Audit job checks `Cargo.lock` against the RustSec Advisory Database on every push and pull request.
- **Code coverage in CI** — `cargo-llvm-cov` generates HTML coverage reports on the Linux x86_64 CI job, uploaded as downloadable artifacts.
- **Weekly fuzz testing** — `cargo-fuzz` target for the DMAP binary protocol parser runs for 5 minutes every Sunday via a scheduled GitHub Actions workflow. Crash artifacts are uploaded automatically.
- **Comprehensive unit test suite** — Added 60+ new test functions (up from 11) covering:
  - `smart_rules.rs` — all text/numeric/date operators, match modes (All/Any), limit by items/duration/size, sort ordering, leap year helper, date cutoff computation.
  - `tag_parser.rs` — `is_audio_file()` for all extensions, case insensitivity, unsupported formats, edge cases.
  - `audio/mod.rs` — `slider_to_pipeline()` curve properties, `redact_url_secrets()` for Plex/Jellyfin/Subsonic tokens, volume path helper.
  - `daap/dmap.rs` — i8/i64 extraction, zero-length strings, nested containers, missing tags, partial headers, max values, UTF-8 strings.
  - `local/engine.rs` — `db_model_to_track()` conversion, None fields, invalid UUID fallback, invalid date handling, `get_mtime()` on nonexistent files.
- **Property-based testing** — `proptest` added as dev-dependency with 3 property tests for the smart rules engine: "All mode ⊆ Any mode", "limiting never increases count", "empty rules returns all tracks".
- **Developer build script flags** — `build-linux.sh` and `build-macos.sh` now support `--check`, `--fmt`, `--clippy`, and `--coverage` quick-exit flags matching the existing Windows `build-windows.ps1 -Check/-Fmt/-Clippy/-Coverage` pattern.
- **Local coverage reporting** — All three build scripts support `--coverage` / `-Coverage` to run `cargo llvm-cov --summary-only` with auto-install. Coverage reports exclude untestable modules (UI, remote backends, DB migrations) via `--ignore-filename-regex` for meaningful percentages.
- **DMAP parser tag coverage** — Added `mikd` (media item kind, I8) and `mper` (persistent ID, I64) to the DMAP tag classification table.

### Fixed
- **Windows Winget/Add-Remove Programs showing version in app name** — Added `AppVerName=Tributary` to the Inno Setup script so the display name is just "Tributary" instead of "Tributary version 0.3.0" in Winget upgrade lists and Add/Remove Programs.

### Changed
- **Windows x86_64 toolchain switched from GCC to Clang/LLVM** — The Windows x86_64 build now uses MSYS2's CLANG64 environment (`mingw-w64-clang-x86_64-*` packages) with the `x86_64-pc-windows-gnullvm` Rust target instead of UCRT64/GCC (`x86_64-pc-windows-gnu`). This unifies both Windows architectures (x86_64 and aarch64) on the same LLVM/Clang compiler family, improving toolchain consistency and enabling better LLVM integration for future sanitizer and profiling work. End-user binaries are identical native Windows PE executables.
- **CI Clippy upgraded to pedantic** — The Linux CI Clippy step now uses `cargo clippy --all-targets -- -D warnings` which picks up the crate-level `#![warn(clippy::pedantic, clippy::nursery)]` configuration. macOS and Windows CI continue to use `-D warnings` (same effect since the crate-level warns are active).
- **CI coverage excludes untestable modules** — The `cargo-llvm-cov` step now uses `--ignore-filename-regex` to exclude UI, remote backend clients, DB migrations, desktop integration, and `main.rs` from coverage reports, providing meaningful coverage percentages for testable core logic.

## [0.3.0] — 2026-04-15

### Added
- **Realtime text search filter** — A search bar above the browser panes filters the tracklist in real-time with case-insensitive substring matching across title, artist, album, and genre. Composes with the existing genre/artist/album browser selections. Search text clears automatically on source switch.
- **Song properties dialog** — Right-click any track → "Properties…" opens a dedicated dialog with editable fields (Title, Artist, Album, Genre, Year, Track #, Disc #) and read-only info (Format, Bitrate, Sample Rate, Duration, File Path). Tags are written safely via `lofty` with explicit Save/Cancel buttons — no inline editing in the tracklist. Supports MP3 (ID3v2), M4A/AAC (MP4 atoms), OGG Vorbis, and FLAC.
- **Batch metadata editing** — When multiple tracks are selected, the Properties dialog shows only batch-appropriate fields (Artist, Album, Genre, Year, Disc #). Fields with mixed values display a "Mixed" placeholder. Only user-modified fields are written; unchanged fields are left untouched per-file.
- **MusicBrainz auto-fill** — Single-track Properties dialog includes a "MusicBrainz Lookup" button that queries the MusicBrainz Recording Search API by title + artist. Results populate the form fields but still require manual Save — no automatic writes.
- **State/Province column for Internet Radio** — Radio stations now display a "State/Province" column (repurposing the Album column) alongside the existing Country column. The Radio-Browser API `state` field is populated for most US and international stations.
- **Tiered "Stations Near Me" search** — Near Me now uses a three-tier search strategy: (1) geo-distance sorted stations with real coordinates, (2) stations matching the user's state/province (catches stations like WBAA that have state metadata but no coordinates), (3) country-only fallback sorted by votes. Results are merged and deduplicated, with nearby stations appearing first.
- **Column drag-and-drop reordering** — Tracklist columns can now be reordered by dragging their headers. Column order is persisted to `config.json` and restored on launch.
- **Local library playlists** — Regular and smart playlists for the local library backend. Playlists survive library folder changes via fingerprint-based track matching (title, artist, album, duration). Smart playlists support iTunes-style rules with 15 filterable fields, text/numeric/date operators, result limiting, and live updating.
- **Fedora COPR installation instructions** — README now documents the `jmsqrd/tributary` COPR repository for one-command Fedora installation.
- **Arch Linux AUR installation instructions** — README now documents the three AUR packages (`tributary`, `tributary-bin`, `tributary-git`).
- **Windows winget installation instructions** — README now documents `winget install jm2.Tributary`.
- **Flathub readiness** — AppStream metainfo now includes `<developer>` tag, `<screenshots>` section, and full release history. Flatpak manifest updated with `xdg-music:rw` permission for tag editing.

### Performance
- **Batch tracklist updates via `splice()`** — `display_tracks()` now uses `ListStore::splice()` to replace all items in a single atomic operation instead of `remove_all()` + N individual `append()` calls. This eliminates multi-second UI freezes when connecting to large remote libraries (e.g., DAAP with thousands of tracks) or switching between sources. The sidebar connection spinner now animates smoothly during the transition.
- **Persistent album art worker thread** — Remote album art fetching now uses a single long-lived background thread with a reusable HTTP client instead of spawning a new `std::thread` (and new TLS session) for every track change. A generation counter discards stale fetches when the user rapidly skips through tracks, eliminating UI lag on remote shares.
- **Shared database connection** — Playlist operations (load, create, rename, delete, add/remove tracks, smart rule editing) now reuse a single SQLite connection via `tokio::sync::OnceCell` instead of opening a new connection and re-running migrations on every action. Eliminates ~9 redundant `init_db()` calls per session.
- **Debounced browser rebuilds during scan** — `TrackUpserted` and `TrackRemoved` events now defer the 3-pane browser rebuild by 500 ms using a generation-counter timer. During initial library scan with many files, this collapses dozens of consecutive rebuilds into a single update, preventing UI lag. The tracklist store is still updated immediately so tracks appear in real-time.
- **Search filter debounce (100 ms)** — The browser search entry now debounces filter callbacks by 100 ms using a generation-counter timer, preventing the expensive filter-and-rebuild from firing on every keystroke during fast typing.

### Fixed
- **About dialog icon missing on macOS `.app` bundles** — The macOS build script now copies the app's hicolor icons (`data/icons/hicolor/`) into the `.app` bundle's `Contents/Resources/share/icons/hicolor/`. Additionally, the icon theme search path now includes the bundle's `Contents/Resources/share/icons` directory at runtime.
- **M4A/MP4 album art not displaying on macOS** — Added a raw MP4 `covr` atom parser as a last-resort fallback when lofty's unified `pictures()` API doesn't surface embedded artwork. This handles Apple-encoded M4A files where the cover art is stored in iTunes-style atoms that lofty's tag abstraction doesn't expose on all platforms.
- **DAAP remote tracks missing album art** — The DAAP backend now constructs per-track cover art URLs (`/databases/{db}/items/{id}/extra_data/artwork`) and sets `cover_art_url` on each track. Previously all DAAP tracks had `cover_art_url: None`, so the header bar always showed the generic placeholder icon.
- **Windows ARM64 build script suggesting x64 MSYS2 packages** — `build-windows.ps1` now auto-detects ARM64 via `RuntimeInformation.ProcessArchitecture` and `PROCESSOR_ARCHITECTURE`, defaulting to `aarch64-pc-windows-gnu` / `clangarm64` instead of always suggesting x64 packages.
- **Blank window on Fedora WSL (Vulkan/dzn driver incompatibility)** — Added WSL detection in `main.rs` (checks `WSL_DISTRO_NAME`, `WSL_INTEROP`, and `/proc/sys/fs/binfmt_misc/WSLInterop`) that forces `GSK_RENDERER=gl` to avoid the broken Vulkan Dozen (dzn) driver path in WSLg.
- **SHA256 checksums only contained Flatpak artifacts** — The release workflow's `upload-artifact` steps were conditional on `workflow_dispatch`, so during `release` events the checksums job couldn't find non-Flatpak artifacts. Made artifact uploads unconditional for all platform jobs (macOS, Windows, DEB, RPM, Arch).
- **Location consent dialog referenced wrong service** — The geolocation consent dialog said "ip-api.com" but the app actually uses a multi-provider HTTPS cascade (`ipapi.co`, `ipwho.is`, `freeipapi.com`). Updated to generic "a geolocation service" text.
- **"Reset to Defaults" only reset column visibility** — The Preferences reset button now also resets column ordering to the default layout.
- **Inno Setup post-install dialog could hang silent installs** — The `[Run]` section used `skipifsilent`, which still showed the "Launch Tributary" checkbox under `/SILENT` (only suppressed under `/VERYSILENT`). Changed to `skipifnotsilent` so the dialog is never shown during any silent install, preventing automation pipeline hangs (e.g. winget).
- **Context menu "Add to Playlist" showed scrollbars** — GTK4's `PopoverMenu::from_model()` wraps submenu sections in an internal `ScrolledWindow` that added unnecessary scrollbars even for menus with only 2–3 entries. Replaced the `append_section` submenu approach with a flat menu structure: a disabled "Add to Playlist" header item followed by indented playlist names as regular clickable items. No submenu = no internal ScrolledWindow = no scrollbars. Previous CSS-only and programmatic `ScrolledWindow` traversal fixes were insufficient.
- **Right-click context menu selected wrong row** — The right-click handler used a hardcoded 25px row height estimate to guess which row was clicked, but didn't account for the column header height, causing consistent off-by-one selection errors. Removed the unreliable row-estimation block entirely — the context menu now operates on whatever row(s) are already selected via normal left-click.

### Changed
- **Windows GSK renderer switched from Vulkan to GL** — The Windows renderer override was changed from `GSK_RENDERER=vulkan` to `GSK_RENDERER=gl` for broader driver compatibility. The GL renderer still provides hardware-accelerated libadwaita animations but avoids Vulkan driver issues on some systems.
- **Geolocation now extracts region/state** — All three geolocation providers (`ipapi.co`, `ipwho.is`, `freeipapi.com`) now return `region` data, enabling state-level radio station filtering.
- **Radio column layout expanded** — Radio mode now shows 6 columns (Title, Country, State/Province, Tags, Bitrate, Format) instead of 5.
- **Browser layout restructured** — The browser is now a vertical Box containing the search entry on top and the horizontal genre/artist/album panes below. `rebuild_browser_data` and `update_browser_visibility` updated accordingly.

### Removed
- **Dead `platform` module** — The Phase 1 stub `src/platform/mod.rs` (with its own `MediaControlsBackend` trait and no-op implementations) was never used and has been superseded by `src/desktop_integration/mod.rs` (souvlaki). Deleted the module and removed `mod platform` from `main.rs`.

## [0.2.1] — 2026-04-05

### Fixed
- **DAAP date modified 31 years in the future** — The DAAP backend was adding a 978,307,200-second epoch offset to `asdm` (song date modified) values, assuming they were relative to the DAAP epoch (2001-01-01). In practice, real-world DAAP servers (forked-daapd, OwnTone, etc.) send standard Unix timestamps. Removed the offset so dates display correctly. No other protocols were affected.
- **Avahi duplicate hostname discovery** — When Avahi services are freshly registered (rather than available at startup), Avahi's conflict resolution can append `-2`, `-3`, etc. to hostnames. The mDNS discovery now strips these suffixes before building the dedup key, preventing duplicate sidebar entries like `myhost-2`.
- **Preferences checkmarks low resolution on Windows/Linux** — The CSS selector was targeting the wrong GTK4 node (`checkbutton indicator` instead of `checkbutton check`). Fixed the selector to target the correct sub-node for crisp rendering across all platforms. Grid layout preserved.
- **Inno Setup installer not fully silent for Winget** — Added `AppId` (stable GUID), `VersionInfoVersion`, `PrivilegesRequiredOverridesAllowed=commandline`, `DisableDirPage=auto`, `DisableProgramGroupPage=auto`, `CloseApplications=yes`, `CloseApplicationsFilter=tributary.exe`, and `SetupLogging=yes` to the Inno Setup script. These directives ensure Winget's `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-` flags work correctly for fully unattended installs.
- **About Tributary icon missing on Windows and Linux (RPM)** — The About dialog referenced icon name `"tributary"` but the hicolor theme files are named `io.github.tributary.Tributary.png`. Changed the icon name to `io.github.tributary.Tributary` and updated the icon theme search paths to look in `data/icons/` (development) and `share/icons/` (installed). The Windows build script now bundles the app's hicolor icons into the dist folder.
- **Failed server connection doesn't revert sidebar selection** — When a server connection attempt fails (timeout, bad credentials, etc.), the sidebar now reverts to the previously selected source instead of staying on the failed entry. A connection guard tracks the pending connection URL and pre-connection sidebar position, clearing the spinner and restoring selection on failure.
- **Remote cover art not loading** — `fetch_remote_album_art()` relied on `tokio::runtime::Handle::try_current()` which silently fails on the GTK main thread (no tokio context). Replaced with a `std::thread` + `reqwest::blocking::Client` pattern (matching the local album art extractor), with a 15-second timeout. No auth tokens are logged.
- **Radio station elapsed time not updating** — The GStreamer position timer required both position and duration to be queryable before emitting `PositionChanged` events. Live radio streams have no finite duration, so position ticks were never sent. Now emits ticks with `duration_ms: 0` for live streams. The UI shows elapsed time and displays "LIVE" instead of a duration, and the progress slider stays inactive. This also fixes the occasional endless buffering spinner on slow-to-load streams, since position ticks now clear the spinner for live streams too.

## [0.2.0] — 2026-04-04

### Added
- **Internet Radio** — New "Internet Radio" sidebar section with three sub-sources:
  - **Top Clicked** — most-clicked stations from the Radio-Browser community database.
  - **Top Voted** — highest-rated stations.
  - **Stations Near Me** — geo-located stations sorted by distance, with user consent prompt for IP-based geolocation (via `ipapi.co` over HTTPS).
  - Dynamic column switching: radio sources show Title, Country, Tags, Bitrate, Codec; music sources restore the full 12-column layout.
  - Browser panes are hidden when viewing radio stations.
  - DNS-based Radio-Browser API mirror resolution (`all.api.radio-browser.info`).
  - Double-click plays the station's stream URL via GStreamer.
- **Manual server addition** — `+` button in the sidebar toolbar opens an "Add Server" dialog with server type dropdown (Subsonic, Jellyfin, Plex), URL, username, and password fields. Servers are persisted to `servers.json` (type, name, URL only — no credentials stored).
- **Manual server deletion** — Trash button on manually-added servers removes them from the sidebar and `servers.json`.
- **Regular discovery refresh** — mDNS `ServiceRemoved` events now remove offline servers from the sidebar. Jellyfin UDP discovery re-broadcasts every 60 seconds; servers are removed after 3 consecutive missed cycles (3 minutes).
- **`DiscoveryEvent` enum** — Discovery channel now carries `Found` and `Lost` variants instead of raw `DiscoveredServer` structs.
- **`manually_added` field** on `SourceObject` — distinguishes user-added servers from auto-discovered ones. Manually-added servers are never auto-removed by discovery refresh.
- **`location_enabled` preference** — Persisted in `config.json` to remember the user's geolocation consent choice.
- **About dialog** — Ptyxis-style `adw::AboutDialog` with app icon, version, author (John-Michael Mulesa), website link to GitHub, and "Report an Issue" link to GitHub Issues.
- **SHA256 checksums** — CI and Release workflows now generate and upload `SHA256SUMS.txt` for all build artifacts.
- **Packit / Fedora COPR** — RPM spec and `.packit.yaml` for automated Fedora COPR builds.

### Changed
- **Sidebar** — Now returns a `gtk::Box` (scrolled list + toolbar) instead of a bare `ScrolledWindow`. The toolbar contains the `+` add-server button.
- **Sidebar categories** — Jellyfin and Plex servers now appear under separate "Jellyfin" and "Plex" sidebar headers instead of the combined "Jellyfin / Plex" category. "Internet Radio" added as the last category.
- **Buffering spinner** — Debounce threshold increased from 100 ms to 300 ms to prevent sub-100 ms blinking on fast-loading local files. Added `PositionChanged`-based fallback: if the elapsed time is advancing (audio is actually playing), the spinner is cleared definitively — fixing the endless spinner bug on some remote streams where GStreamer never emits a clean `Playing` state transition after buffering.
- **Jellyfin UDP discovery** — Now runs in a continuous loop (60-second intervals) instead of a single broadcast, enabling dynamic server detection and removal.

### Security
- **HTTPS geolocation** — Multi-provider HTTPS cascade (`ipapi.co` → `ipwho.is` → `freeipapi.com`) for IP-based geolocation. All providers are reputable, global, and require no API keys.
- **Request timeouts** — All Radio-Browser API and geolocation HTTP requests now have a 15-second timeout to prevent indefinite hangs. DAAP logout requests now use a 5-second timeout.
- **Stream URL validation** — Radio station stream URLs are filtered to only allow `http://` and `https://` schemes, preventing `file://` or other scheme injection from malicious Radio-Browser entries.
- **Auth token redaction in logs** — Added `redact_url_secrets()` utility that masks `X-Plex-Token`, `api_key` (Jellyfin), and Subsonic `t`/`s` (token/salt) query parameters before they reach log output. Applied to all debug-level request logging in Plex, Subsonic, and Jellyfin clients, and to the GStreamer `load_uri` info log. Subsonic salt (`s`) is only redacted when the token param (`t`) is also present, avoiding false positives on unrelated URLs.
- **SQLite WAL mode** — Enabled `PRAGMA journal_mode=WAL` and `PRAGMA busy_timeout=5000` on database init for safer concurrent access and reduced `SQLITE_BUSY` errors.

### UI Polish
- **Preferences checkmarks** — Added CSS rule to fix low-resolution checkmark indicators on Windows (`checkbutton indicator` forced to 18×18px with explicit icon size).
- **Radio column rename** — The "Artist" column is dynamically renamed to "Country" when viewing radio stations, and restored to "Artist" when switching back to music sources.
- **Stations Near Me accuracy** — Added `has_geo_info=true` filter to Radio-Browser queries so only stations with actual coordinates are returned. Country code from geolocation is passed as `countrycode` filter for dramatically improved local relevance.

### Removed
- **Keyboard Shortcuts** menu item removed from the hamburger menu (was non-functional).

### Fixed
- Endless playback spinner on certain remote source tracks.
- Spinner blinking at sub-100 ms intervals on local file playback.
- `remove_empty_category_header` was previously dead code — now actively used by discovery `Lost` events.
- **Runtime panic in `fetch_remote_album_art`** — `tokio::task::spawn` was called from the GTK main thread which has no tokio runtime context. Replaced with `Handle::try_current()` guard to safely spawn on the background runtime.
- **Album art blocking GTK main thread** — `update_album_art()` now extracts embedded picture bytes on a background `std::thread`, sending results back via `async_channel`. Previously, `lofty::read_from_path()` ran synchronously on the GTK thread, causing UI freezes on large FLAC files.
- **Tokio runtime premature shutdown** — Replaced `tokio::signal::ctrl_c()` parking with `std::future::pending()`. The `ctrl_c` approach was unreliable on Windows without a console and could drop in-flight async tasks if Ctrl+C fired before GTK exited.
- **`DaapBackend::Drop` panic on shutdown** — The `Drop` impl now guards `tokio::task::spawn` with `Handle::try_current()` to avoid panicking when the runtime has already been dropped during process teardown.
- **Double-wrapped LIKE pattern in local search** — `LocalBackend::search()` was passing `%query%` to SeaORM's `.contains()` which adds its own `%` wrapping, producing `%%query%%`. Now passes the raw query string.
- **Unsafe `.unwrap()` on `ColumnViewColumn` downcast** — `restore_sort_state()` now uses `let Some(col) = ... else { continue }` instead of `.unwrap()`.
- **Settings directory not pre-created** — `save_volume()` and all window settings persistence (`save_repeat_mode`, `save_shuffle`, `save_sort_state`) now ensure the `<data_dir>/tributary/` directory exists via `create_dir_all` before writing, preventing silent failures on first launch before the database initialises.

---

## [0.1.0] — 2026-04-02

### Added

#### Core Architecture
- Project skeleton with `MediaBackend` async trait, core data models (`Track`, `Album`, `Artist`, `SearchResults`, `LibraryStats`), and structured `BackendError` types.
- GTK4 / libadwaita application with reverse-DNS ID `io.github.tributary.Tributary`.
- Tokio background runtime for async I/O, bridged to the GTK main thread via `async_channel`.

#### UI
- **Rhythmbox-style layout** — three-pane design with sidebar, browser filter, and tracklist.
- **GtkColumnView tracklist** — 12 resizable, sortable columns: #, Title, Time, Artist, Album, Genre, Year, Date Modified, Bitrate, Sample Rate, Plays, Format.
- **Three-state column sorting** — ascending → descending → none (clear sort), with sort state persisted across launches.
- **Browser filtering** — Genre → Artist → Album cascading filter panes with "All" entries and item counts.
- **Sidebar** — source list with category headers (Local, Subsonic, Jellyfin, Plex, DAAP), discovered server entries with lock/unlock icons, connecting spinners, and DAAP eject (disconnect) button.
- **Header bar** — playback controls (prev/play-pause/next), repeat (off/all/one) and shuffle toggles, now-playing widget with album art + title + artist, progress scrubber with position/duration labels, volume slider with cubic perceptual curve, and hamburger menu.
- **Album art** — extracted from embedded tags (ID3, Vorbis, MP4/M4A `covr` atom) and displayed in the header bar.
- **Status bar** — track count and total duration summary.
- Light & dark mode via libadwaita automatic color scheme.

#### Local Backend
- SQLite persistence via SeaORM with automatic migration.
- `lofty` audio tag parsing for metadata extraction.
- `notify` real-time filesystem watching with debounced event handling.
- Async scanning engine with progress reporting.
- `date_modified` from filesystem mtime, used for incremental re-scan.

#### Remote Backends
- **Subsonic / Navidrome** — REST/JSON client with MD5 token authentication, full library fetch (artists → albums → songs), search, cover art URLs, and authenticated streaming URLs.
- **Jellyfin** — REST/JSON client with API key and username/password authentication, music library discovery, paginated item fetching, cover art and streaming URLs.
- **Plex** — REST/JSON client with X-Plex-Token and plex.tv sign-in authentication, music library section discovery, track/album/artist fetching, thumbnail and streaming URLs.
- **DAAP / iTunes Sharing** — DMAP binary TLV protocol parser (nom-based, 24+ tag types), 5-step HTTP session handshake (server-info → login → update → databases → items), password-only auth dialog, session-id streaming URLs, best-effort logout on disconnect.

#### Discovery
- **mDNS** — zero-config discovery for `_subsonic._tcp`, `_plexmediasvr._tcp`, and `_daap._tcp` services via `mdns-sd`.
- **Jellyfin UDP** — broadcast discovery on port 7359 with JSON response parsing.
- **DAAP password probe** — background `/server-info` check to determine if a share requires authentication (lock icon vs. open icon in sidebar).
- Automatic sidebar population with deduplication.

#### Audio Playback
- GStreamer `playbin3` pipeline (with `playbin` fallback) for local and remote stream playback.
- Play/pause, stop, seek, next/previous track controls.
- Shuffle (random, avoid repeat) and repeat (off/all/one) modes with persistence.
- Volume control with cubic perceptual curve and persistence across launches.
- Buffering state detection with debounced spinner display.
- End-of-stream auto-advance with shuffle/repeat awareness.

#### Desktop Integration
- **MPRIS** (Linux) / **SMTC** (Windows) / **MPNowPlayingInfoCenter** (macOS) via `souvlaki` — play/pause/stop/next/previous media key support, now-playing metadata updates.
- HWND extraction on Windows for SMTC initialization.

#### Packaging & CI
- **Linux** — `.deb` (Debian/Ubuntu), `.rpm` (Fedora), Arch Linux `PKGBUILD`, Flatpak manifest.
- **macOS** — `.app` bundle with rpath-fixed dylibs, ad-hoc code signing, `.dmg` packaging. Runtime environment setup for bundled GTK/GStreamer resources.
- **Windows** — MSYS2 UCRT64 DLL bundling, `.zip` portable package, Inno Setup `.exe` installer. Bundled GStreamer plugin path detection.
- **CI** — GitHub Actions workflows for Linux (x86_64 + aarch64), macOS (aarch64), Windows (x86_64 + aarch64) with clippy, rustfmt, build, and test.
- **Release** — automated release workflow producing Flatpak, DMG, Windows zip + installer, deb, rpm, and Arch packages for all supported architectures.

#### Other
- Custom CSS stylesheet for data-table styling and album art placeholder.
- App icon (PNG + ICO + macOS iconset).
- `.desktop` file and AppStream metainfo for Linux desktop integration.
- Windows resource file with icon embedding.

[0.3.0]: https://github.com/jm2/tributary/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/jm2/tributary/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jm2/tributary/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jm2/tributary/releases/tag/v0.1.0
