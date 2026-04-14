# Changelog

All notable changes to Tributary are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] — Unreleased

### Added
- **State/Province column for Internet Radio** — Radio stations now display a "State/Province" column (repurposing the Album column) alongside the existing Country column. The Radio-Browser API `state` field is populated for most US and international stations.
- **Tiered "Stations Near Me" search** — Near Me now uses a three-tier search strategy: (1) geo-distance sorted stations with real coordinates, (2) stations matching the user's state/province (catches stations like WBAA that have state metadata but no coordinates), (3) country-only fallback sorted by votes. Results are merged and deduplicated, with nearby stations appearing first.
- **Column drag-and-drop reordering** — Tracklist columns can now be reordered by dragging their headers. Column order is persisted to `config.json` and restored on launch.
- **Local library playlists** — Regular and smart playlists for the local library backend. Playlists survive library folder changes via fingerprint-based track matching (title, artist, album, duration). Smart playlists support iTunes-style rules with 15 filterable fields, text/numeric/date operators, result limiting, and live updating.
- **Fedora COPR installation instructions** — README now documents the `jmsqrd/tributary` COPR repository for one-command Fedora installation.
- **Arch Linux AUR installation instructions** — README now documents the three AUR packages (`tributary`, `tributary-bin`, `tributary-git`).
- **Windows winget installation instructions** — README now documents `winget install jm2.Tributary`.

### Fixed
- **SHA256 checksums only contained Flatpak artifacts** — The release workflow's `upload-artifact` steps were conditional on `workflow_dispatch`, so during `release` events the checksums job couldn't find non-Flatpak artifacts. Made artifact uploads unconditional for all platform jobs (macOS, Windows, DEB, RPM, Arch).
- **Location consent dialog referenced wrong service** — The geolocation consent dialog said "ip-api.com" but the app actually uses a multi-provider HTTPS cascade (`ipapi.co`, `ipwho.is`, `freeipapi.com`). Updated to generic "a geolocation service" text.
- **"Reset to Defaults" only reset column visibility** — The Preferences reset button now also resets column ordering to the default layout.
- **Inno Setup post-install dialog could hang silent installs** — The `[Run]` section used `skipifsilent`, which still showed the "Launch Tributary" checkbox under `/SILENT` (only suppressed under `/VERYSILENT`). Changed to `skipifnotsilent` so the dialog is never shown during any silent install, preventing automation pipeline hangs (e.g. winget).

### Changed
- **Geolocation now extracts region/state** — All three geolocation providers (`ipapi.co`, `ipwho.is`, `freeipapi.com`) now return `region` data, enabling state-level radio station filtering.
- **Radio column layout expanded** — Radio mode now shows 6 columns (Title, Country, State/Province, Tags, Bitrate, Format) instead of 5.

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

[0.3.0]: https://github.com/jm2/tributary/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/jm2/tributary/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/jm2/tributary/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jm2/tributary/releases/tag/v0.1.0
