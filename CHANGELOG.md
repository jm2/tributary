# Changelog

All notable changes to Tributary are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] — v0.2.0

### Added
- **About dialog** — Ptyxis-style `adw::AboutDialog` with app icon, version, author (John-Michael Mulesa), website link to GitHub, and "Report an Issue" link to GitHub Issues.
- **SHA256 checksums** — CI and Release workflows now generate and upload `SHA256SUMS.txt` for all build artifacts.
- **Packit / Fedora COPR** — RPM spec and `.packit.yaml` for automated Fedora COPR builds.

### Changed
- **Sidebar categories** — Jellyfin and Plex servers now appear under separate "Jellyfin" and "Plex" sidebar headers instead of the combined "Jellyfin / Plex" category.
- **Buffering spinner** — Debounce threshold increased from 100 ms to 300 ms to prevent sub-100 ms blinking on fast-loading local files. Added `PositionChanged`-based fallback: if the elapsed time is advancing (audio is actually playing), the spinner is cleared definitively — fixing the endless spinner bug on some remote streams where GStreamer never emits a clean `Playing` state transition after buffering.

### Removed
- **Keyboard Shortcuts** menu item removed from the hamburger menu (was non-functional).

### Fixed
- Endless playback spinner on certain remote source tracks.
- Spinner blinking at sub-100 ms intervals on local file playback.

---

## [0.1.0] — 2026-03-28

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

[Unreleased]: https://github.com/jm2/tributary/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jm2/tributary/releases/tag/v0.1.0
