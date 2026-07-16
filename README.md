<img src="data/tributary.png" width="96" alt="Tributary icon">

# Tributary

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![CI](https://github.com/jm2/tributary/actions/workflows/ci.yml/badge.svg)](https://github.com/jm2/tributary/actions)

A high-performance, **Rhythmbox-style** media manager written in pure Rust with **GTK4** and **libadwaita**.

Tributary provides a unified interface for managing and streaming music from multiple sources — local files, Subsonic/Navidrome, Jellyfin, Plex, DAAP/iTunes shares, and internet radio — all through a single, responsive library view.

![Tributary Main Interface](data/screenshot.png)

## Features

| Feature | Status |
|---------|--------|
| GTK4 / libadwaita UI (Rhythmbox-style `GtkColumnView`) | ✅ |
| Browser filtering (Genre → Artist → Album) | ✅ |
| Local library with FS `date_modified` scanning | ✅ |
| Real-time filesystem watching (`notify`) | ✅ |
| SQLite persistence (`SeaORM`) | ✅ |
| GStreamer audio playback (`playbin3`) | ✅ |
| MPRIS / SMTC / macOS Now Playing integration (`souvlaki`) | ✅ |
| Playback controls (play/pause, next/prev, seek, volume) | ✅ |
| Shuffle & repeat (off / all / one) with persistence | ✅ |
| Column sort persistence | ✅ |
| Subsonic / Navidrome / Nextcloud Music backend | ✅ |
| Jellyfin backend | ✅ |
| Plex backend | ✅ |
| DAAP / iTunes Sharing backend (DMAP binary protocol) | ✅ |
| mDNS zero-config discovery (Subsonic, Plex, DAAP) | ✅ |
| Jellyfin UDP broadcast discovery | ✅ |
| DAAP sidebar eject button (disconnect) | ✅ |
| Password-only auth dialog (DAAP) | ✅ |
| Regular discovery refresh (add/remove servers dynamically) | ✅ |
| Manual server addition/deletion with `servers.json` persistence | ✅ |
| Internet Radio (Top Clicked, Top Voted, Stations Near Me) | ✅ |
| Tiered geo-location (geo-distance → state → country) | ✅ |
| Column drag-and-drop reordering with persistence | ✅ |
| Regular & smart playlists (iTunes-style rules engine) | ✅ |
| Realtime text search filter (title, artist, album, genre) | ✅ |
| Song metadata editing (Properties dialog with Save/Cancel) | ✅ |
| Batch metadata editing (multi-select) | ✅ |
| MusicBrainz auto-fill lookup | ✅ |
| Keyboard shortcut: `Ctrl+F` / `Cmd+F` to search | ✅ |
| XDG music directory support (non-English locales) | ✅ |
| Network connection guard (prevents duplicate auth) | ✅ |
| i18n/l10n framework (13 languages, auto locale detection) | ✅ |
| Audio output selector (local + MPD, iTunes AirPlay-style) | ✅ |
| MPD output backend (sink-only, TCP with security hardening) | ✅ |
| Output switching (click to swap local ↔ MPD) | ✅ |
| AirPlay 1 (RAOP) output | ✅ Requires GStreamer's `raopsink` element (ships in `gst-plugins-bad`); a missing element fails with an actionable install message |
| AirPlay 2 / HomeKit output | ❌ Not yet supported — see [AirPlay 2 roadmap](#airplay-2-roadmap) below |
| Chromecast output (Cast V2 — local files + remote sources) | ✅ |
| Album artist sort (preference toggle) | ✅ |
| Smart playlist compound sort (multi-key ordering) | ✅ |
| Geo-distance sorting for Stations Near Me | ✅ |
| USB/removable-media browsing (live native sidebar + bounded track scan) | ✅ |
| USB file transfer (copy to device with progress) | ❌ Planned ([#8](https://github.com/jm2/tributary/issues/8)) |
| Multiple music library directories | ✅ |
| Playlist import/export (XSPF) | ✅ |
| Default smart playlists (Recently Added, Recently Played, Top 25) | ✅ |
| Window position persistence | ✅ |
| Windows 11 Snap Layout support | ✅ |
| Linux and macOS file associations | ✅ |
| Cross-platform: Linux, macOS, Windows | ✅ |
| Light & dark mode | ✅ Automatic (libadwaita) |

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                   GTK4 / libadwaita UI              │
│     (GtkColumnView, Browser, Sidebar, HeaderBar)    │
├─────────────────────────────────────────────────────┤
│              MediaBackend trait (async)             │
├──────────┬──────────┬───────────┬──────┬────────────┤
│  Local   │ Subsonic │  Jellyfin │ Plex │    DAAP    │
│ (SQLite) │ (REST)   │  (REST)   │(REST)│(DMAP/mDNS) │
├──────────┴──────────┴───────────┴──────┴────────────┤
│           GStreamer (audio pipeline)                │
├─────────────────────────────────────────────────────┤
│    Platform: MPRIS │ SMTC │ MPNowPlayingInfoCenter  │
└─────────────────────────────────────────────────────┘
```

The diagram above is the **intended** architecture, and the remote backends (Subsonic, Jellyfin, Plex, DAAP) each implement the `MediaBackend` async trait. The trait is not yet the real seam, though: it is never used as a trait object, the UI still branches per backend when connecting a source, and the local library is queried directly through SQLite rather than through `LocalBackend`. Unifying them is tracked as P3.2 in [`docs/task.md`](docs/task.md).

---

## Installation

### Fedora (COPR)

Tributary is available from the [jmsqrd/tributary](https://copr.fedorainfracloud.org/coprs/jmsqrd/tributary/) COPR repository:

```bash
sudo dnf copr enable jmsqrd/tributary
sudo dnf install tributary
```

### Arch Linux (AUR)

Tributary is available on the [AUR](https://aur.archlinux.org/) in three variants:

| Package | Description |
|---------|-------------|
| [`tributary`](https://aur.archlinux.org/packages/tributary) | Build from the latest release source |
| [`tributary-bin`](https://aur.archlinux.org/packages/tributary-bin) | Pre-built binary from the latest release |
| [`tributary-git`](https://aur.archlinux.org/packages/tributary-git) | Build from the latest `main` branch commit |

Install with your preferred AUR helper, for example:

```bash
yay -S tributary-bin
```

### Windows (winget)

Tributary is available via [winget](https://learn.microsoft.com/en-us/windows/package-manager/winget/):

```powershell
winget install jm2.Tributary
```

### Other Platforms

Pre-built packages for Linux (Flatpak, .deb, .rpm), macOS (.dmg), and Windows (.exe installer, .zip) are also available on the [Releases](https://github.com/jm2/tributary/releases) page.

> **macOS note:** The macOS `.dmg` is ad-hoc signed but not notarized, so macOS Gatekeeper will block it on first launch. After mounting the DMG and dragging Tributary to Applications, run:
> ```bash
> xattr -cr /Applications/Tributary.app
> ```
> Then open normally. This is only needed once.

---

## Building from Source

### Prerequisites (all platforms)

- [Rust 1.92+](https://rustup.rs) (stable toolchain) — this is the declared MSRV in `Cargo.toml`, set by the gtk-rs 0.11 release series and verified by a dedicated CI job
- **GTK 4.16+** and **libadwaita 1.6+** — the crate compiles against these API levels, so older
  runtimes will fail to build, not merely fail at startup
- `pkg-config`

### Linux

> **Check your GTK version first:** `pkg-config --modversion gtk4`. Debian 12 and Ubuntu 24.04
> ship GTK 4.8/4.14 and libadwaita below 1.6, so the packages below are not sufficient on those
> releases — you will need a newer distribution, backports, or the Flatpak build.

**Debian / Ubuntu:**
```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libdbus-1-dev pkg-config build-essential
```

**Fedora:**
```bash
sudo dnf install gtk4-devel libadwaita-devel gstreamer1-devel dbus-devel pkgconf-pkg-config gcc
```

**Arch Linux:**
```bash
sudo pacman -S gtk4 libadwaita gstreamer dbus pkgconf base-devel
```

Then build:
```bash
cargo build --release
# or use the helper script:
./scripts/build-linux.sh
```

The binary is at `target/release/tributary`.

### macOS

Requires [Homebrew](https://brew.sh):

```bash
brew install gtk4 libadwaita pkg-config gstreamer gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav adwaita-icon-theme
cargo build --release
```

To create a `.app` bundle and `.dmg`:
```bash
brew install create-dmg   # optional, for DMG packaging
./scripts/build-macos.sh --dmg
```

The app bundle is at `dist/Tributary.app`, and the DMG at `dist/Tributary.dmg`.

> **Note:** The `.app` bundle includes rpath-fixed dylibs and is ad-hoc code-signed so it can run without Homebrew on the target machine. For distribution, proper Apple Developer code signing and notarization are recommended.

### Windows

Requires [MSYS2](https://www.msys2.org) with the CLANG64 environment:

```powershell
# In an MSYS2 CLANG64 shell:
pacman -S mingw-w64-clang-x86_64-gtk4 \
          mingw-w64-clang-x86_64-libadwaita \
          mingw-w64-clang-x86_64-gstreamer \
          mingw-w64-clang-x86_64-gst-plugins-good \
          mingw-w64-clang-x86_64-gst-plugins-bad \
          mingw-w64-clang-x86_64-gst-libav \
          mingw-w64-clang-x86_64-pkg-config \
          mingw-w64-clang-x86_64-toolchain
```

Then, in PowerShell:
```powershell
# Ensure Rust's LLVM target is installed:
rustup target add x86_64-pc-windows-gnullvm

# Build and bundle DLLs:
.\scripts\build-windows.ps1
```

This produces `dist/tributary-windows.zip` with the executable and all required DLLs/resources.

### Flatpak (Linux)

The manifest builds offline (`CARGO_NET_OFFLINE=true`) from a generated
`build-aux/flatpak/cargo-sources.json`. The repository vendors the immutably pinned Cargo source
generator, and the shared helper verifies its recorded checksum before writing the ignored source
manifest beside the Flatpak manifest. Local builds and CI therefore run the same generator from the
same location.

```bash
# Install the tools and configure Flathub for this user:
sudo apt install flatpak flatpak-builder python3-venv
flatpak remote-add --if-not-exists --user flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo

# Keep the generator dependencies isolated from the system Python:
FLATPAK_VENV="${XDG_CACHE_HOME:-$HOME/.cache}/tributary-flatpak-venv"
python3 -m venv "$FLATPAK_VENV"
source "$FLATPAK_VENV/bin/activate"
python3 -m pip install --requirement build-aux/flatpak/generator-requirements.txt

# Verify the vendored pin and generate the offline source manifest:
bash build-aux/flatpak/generate-cargo-sources.sh

# Build and install locally:
flatpak-builder --user --install-deps-from=flathub --force-clean --repo=repo --install \
  build-dir build-aux/flatpak/io.github.tributary.Tributary.yml
```

`./scripts/build-linux.sh --flatpak` uses this same helper and enters the sandboxed build without
first requiring a native Rust/GTK build. The manifest's directory source excludes known VCS,
agent, and generated build/package paths—including `target/`, coverage, and stale source-manifest
output—so those host artifacts are not copied into the SDK build. The local single-file bundle
records Flathub as its runtime repository. The vendored file's
immutable upstream revision, license, checksum, and update procedure are recorded in
`build-aux/flatpak/flatpak-cargo-generator.PROVENANCE`.

The sandbox deliberately does not expose the whole home directory. XDG Music is available
read/write. A custom library selected explicitly in **Preferences → Library Folders** goes through
GTK's [file-chooser portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileChooser.html),
which requests a persistent read/write sandbox grant. Tag editing works when the selected directory
is also writable under the host filesystem's ordinary permissions; a portal cannot make read-only
storage writable.

A custom path saved by an older Flatpak build as a direct host path may become unavailable under
the narrower sandbox policy. Do not remove and re-add that root if preserving track IDs, history,
and playlist links matters: use that root's **Reauthorize…** action in Preferences to select the
same logical folder through the portal, confirm the identity-preserving move, and restart so the
guarded relocation completes before scanning.

Following [Flatpak's external-drive guidance](https://docs.flatpak.org/en/latest/sandbox-permissions.html#external-drive-access),
host-mounted media under `/media`, `/run/media`, and `/mnt` is exposed read-only for the automatic
**Devices** inventory and playback. The `org.gtk.vfs.*` bus namespace makes the host GVfs service
methods available to GIO; Tributary consumes its cached native mount inventory and does not expose
the non-native GVfs filesystem sockets. It does not request raw USB-device access, the whole host
filesystem, or a writable external-media root. To treat external media as a writable custom
library, select that directory explicitly in Preferences and use its portal-backed library entry
rather than the automatic Devices entry. The grant still cannot override a physically or
host-permission read-only filesystem. The automatic Devices entry remains browse/play-only at the
sandbox boundary. Properties checks the selected files and their containing directory on a worker
before enabling its editing controls, so a read-only automatic device is explained before Save and
points to the custom-library flow that can request portal write access.

---

## Running

```bash
# From a release build:
./target/release/tributary

# With debug logging:
RUST_LOG=tributary=debug ./target/release/tributary

# With trace-level logging:
RUST_LOG=tributary=trace ./target/release/tributary
```


---

## Development

### Git Hooks

Tributary includes a pre-commit hook that runs `cargo fmt --check` to prevent formatting errors from being committed. To enable it after cloning:

```bash
git config core.hooksPath hooks
```

### Developer Build Scripts

All three platform build scripts support quick-exit modes for formatting, type-checking, and linting:

```bash
# Linux / macOS:
./scripts/build-linux.sh --fmt       # or build-macos.sh --fmt
./scripts/build-linux.sh --check     # or build-macos.sh --check
./scripts/build-linux.sh --clippy    # or build-macos.sh --clippy
```

```powershell
# Windows (PowerShell):
.\scripts\build-windows.ps1 -Fmt
.\scripts\build-windows.ps1 -Check
.\scripts\build-windows.ps1 -Clippy
.\scripts\build-windows.ps1 -Test
.\scripts\build-windows.ps1 -Run
```

Clippy runs with `clippy::pedantic` and `clippy::nursery` enabled crate-wide (configured in `src/main.rs`).

### Testing & Code Quality

```bash
# Run all tests (unit + proptest property-based):
cargo test

# Quick coverage summary (requires cargo-llvm-cov):
cargo llvm-cov --summary-only

# Full HTML coverage report:
cargo llvm-cov --html --output-dir coverage
```

CI automatically runs on every push/PR:
- **Security audit** — `cargo audit` checks dependencies against the RustSec Advisory Database
- **Pedantic Clippy** — `clippy::pedantic` + `clippy::nursery` with `-D warnings`
- **Code coverage** — `cargo-llvm-cov` HTML report uploaded as a CI artifact (Linux x86_64)
- **Weekly fuzzing** — `cargo-fuzz` target for the DMAP binary parser (5 min, Sundays)

---

## Project Structure

```
src/
├── main.rs                 # Application entry point (GTK + tokio bootstrap)
├── discovery.rs            # mDNS + UDP zero-config server discovery
├── architecture/
│   ├── mod.rs              # Module root & re-exports
│   ├── models.rs           # Track, Album, Artist, SearchResults, LibraryStats
│   ├── backend.rs          # MediaBackend async trait
│   └── error.rs            # BackendError (thiserror)
├── audio/
│   ├── mod.rs              # GStreamer Player (playbin3, bus watch, position timer)
│   ├── output.rs           # AudioOutput trait abstraction
│   ├── local_output.rs     # Local GStreamer playback (AudioOutput impl)
│   ├── mpd_output.rs       # MPD TCP output (AudioOutput impl)
│   ├── airplay_output.rs   # AirPlay/RAOP output (scaffolding)
│   ├── chromecast_output.rs# Chromecast/Cast V2 output (local + remote)
│   └── cast_http_server.rs # Embedded LAN-only HTTP server for Chromecast
├── db/
│   ├── mod.rs              # Database layer root
│   ├── connection.rs       # SQLite init, XDG paths, migration runner
│   ├── entities/
│   │   └── track.rs        # SeaORM entity for tracks table
│   └── migration/
│       └── m20250101_000001_create_tables.rs
├── desktop_integration/
│   └── mod.rs              # OS media controls via souvlaki (MPRIS/SMTC/Now Playing)
├── local/
│   ├── mod.rs              # Local backend root
│   ├── backend.rs          # MediaBackend impl (LocalBackend)
│   ├── engine.rs           # Async scan + notify FS watcher + LibraryEvent channel
│   ├── tag_parser.rs       # lofty audio tag extraction
│   ├── tag_writer.rs       # lofty audio tag writing (MP3, M4A, OGG, FLAC)
│   ├── playlist_manager.rs # Regular + smart playlist CRUD
│   ├── playlist_io.rs      # XSPF playlist import/export with fingerprint matching
│   └── smart_rules.rs      # iTunes-style smart playlist rules engine
├── subsonic/
│   ├── mod.rs              # Subsonic backend root
│   ├── api.rs              # JSON response types (Subsonic REST API)
│   ├── client.rs           # HTTP client (token + legacy auth, request building)
│   └── backend.rs          # MediaBackend impl (in-memory cache)
├── jellyfin/
│   ├── mod.rs              # Jellyfin backend root
│   ├── api.rs              # JSON response types (Jellyfin REST API)
│   ├── client.rs           # HTTP client (API key auth, username/password auth)
│   └── backend.rs          # MediaBackend impl (in-memory cache)
├── plex/
│   ├── mod.rs              # Plex backend root
│   ├── api.rs              # JSON response types (Plex REST API)
│   ├── client.rs           # HTTP client (X-Plex-Token, plex.tv sign-in)
│   └── backend.rs          # MediaBackend impl (in-memory cache)
├── daap/
│   ├── mod.rs              # DAAP backend root
│   ├── dmap.rs             # DMAP binary TLV parser (nom-based, 24 tag types)
│   ├── client.rs           # HTTP client (5-step session handshake)
│   └── backend.rs          # MediaBackend impl (in-memory cache)
├── device/
│   ├── mod.rs              # DeviceInfo model for mounted browseable media
│   └── usb.rs              # GIO mount filtering + logical removable-source identity
├── radio/
│   ├── mod.rs              # Internet Radio module root
│   ├── api.rs              # RadioStation + GeoLocation serde types
│   ├── client.rs           # Radio-Browser API client (DNS mirror, geolocation)
│   └── geo.rs              # Haversine distance + US state/country centroid tables
└── ui/
    ├── mod.rs              # UI module root
    ├── window.rs           # Main window orchestration (GTK lifecycle + event wiring)
    ├── window_state.rs     # Shared WindowState struct (Rc/RefCell state bundle)
    ├── source_connect.rs   # Sidebar selection handler (source switching + auth flows)
    ├── removable_media.rs  # Native mount monitoring + live sidebar reconciliation
    ├── discovery_handler.rs# mDNS/DNS-SD event handler (sidebar + output list)
    ├── context_menu.rs     # Tracklist right-click menu (playlist ops + properties)
    ├── playlist_actions.rs # Playlist CRUD (create, rename, delete, reorder)
    ├── output_switch.rs    # Output selector click handler (local/MPD/AirPlay/Cast)
    ├── header_bar.rs       # Playback controls, now-playing, progress, volume
    ├── sidebar.rs          # Source list (local + remote + discovered + eject)
    ├── browser.rs          # Search bar + Genre → Artist → Album filter panes
    ├── tracklist.rs        # GtkColumnView track listing
    ├── properties_dialog.rs# Song properties editor (single + batch + MusicBrainz)
    ├── playlist_editor.rs  # Smart playlist rules editor dialog
    ├── preferences.rs      # Preferences dialog (library path, browser, columns)
    ├── output_dialogs.rs   # Add Output dialog + outputs.json persistence
    ├── server_dialogs.rs   # Add/auth server dialogs + servers.json persistence
    ├── album_art.rs        # Album art extraction (embedded tags + remote fetch)
    ├── playback.rs         # Playback context + track advance logic
    ├── persistence.rs      # Settings persistence (sort, shuffle, repeat, CSS)
    ├── radio.rs            # Radio-specific UI helpers (column switching, geo-sort)
    ├── dummy_data.rs       # Default sidebar source entries
    ├── style.css           # Custom CSS overrides
    └── objects/
        ├── mod.rs          # GObject wrappers root
        ├── track_object.rs # GObject wrapper for track rows
        ├── source_object.rs# GObject wrapper for sidebar sources
        └── browser_item.rs # GObject wrapper for browser filter items

scripts/
├── build-linux.sh          # Linux build + packaging helper
├── build-macos.sh          # macOS .app/.dmg builder (rpath fix + code sign)
└── build-windows.ps1       # Windows DLL bundler + Inno Setup

build-aux/
├── arch/PKGBUILD           # Arch Linux package definition
├── flatpak/                # Flatpak manifest
└── inno/tributary.iss      # Windows Inno Setup installer script

data/                        # .desktop, AppStream metainfo, icons
```

---

## Usage

### Browsing Your Library

On first launch, Tributary scans your `~/Music` folder (configurable in Preferences) and displays all discovered tracks in the main tracklist. Use the **browser panes** above the tracklist to filter by Genre → Artist → Album. Click any column header to sort; click again to reverse; click a third time to clear the sort.

### Browsing Removable Media

Tributary reads cached native mount metadata from GIO's `VolumeMonitor` on the GTK main thread. It
does not scan platform mount directories, enumerate drive letters, canonicalize paths, or perform
filesystem probes during discovery. Shadowed roots and roots without native-path access are
excluded, as are mounts the backend explicitly classifies as network or loop. A native-path mount
is shown when the platform reports a removable drive, eject or unmount support, or the `device`
volume class. Because class metadata is optional and `can_unmount` is broad, this best-effort policy
can also include a non-removable or natively mounted network filesystem. The translated **Devices**
heading exists only while at least one qualifying mount is present.

The monitor reconciles mount-added, mount-changed, pre-unmount, and mount-removed notifications for
the life of the window. A device keeps its best available logical source key separate from its
native `PathBuf`: mount UUID is preferred, then volume UUID, Unix device identifier, and finally
root URI.
Rows sharing that key are deduplicated deterministically. A rename atomically replaces the row at
the same position. Relocation of the same logical source retires its old scan and playback state,
temporarily falls back to Local, and reselects it at the new path only if that exact automatic
fallback is still current. A confirmed removal retires the matching path synchronously before the
coalesced snapshot, so a rapid same-path reattach cannot preserve stale state; pre-unmount alone
does not remove the row because the operation can fail. Unplugging an active source invalidates its
pending navigation, cached tracks, and playback before removing the row and falling back to Local.

Selecting an uncached device starts a named background scan that streams parsed tracks through a
bounded channel. Its empty projection is installed immediately, so rows from the previous source
cannot be queued under the device identity while the scan runs. GTK checks source ownership while
consuming it. Re-selecting the same logical device, relocating it, or unplugging it retires the old
generation and closes the receiver so the producer stops cooperatively; navigating to a different
source instead lets the newest scan finish into its cache without rendering it. The walk is lazy
and does not follow directory or file symlinks, so links cannot escape the selected volume. It can
still traverse a real nested mount, and cancellation cannot interrupt the one filesystem or
tag-parser operation currently in progress.

The key is best-available logical identity, not proof of unique physical hardware: cloned
filesystems may share a UUID, Unix-device and root-URI fallbacks can change with device/path
assignment, and GIO's broad `can_unmount` signal can include a non-removable or native-path network
mount when the backend supplies no class. Tributary does not mount unmounted volumes, eject
devices, or browse MTP-only devices. Real USB add/change/unplug behavior still needs cross-platform
hardware validation. In Flatpak, the inventory can still list an eligible native mount elsewhere,
but automatic Devices file access is read-only and limited to `/media`, `/run/media`, and `/mnt`;
an inaccessible listed root cannot be scanned or played. Writable custom libraries require
explicit selection through the folder portal as described in [Flatpak (Linux)](#flatpak-linux).
The remaining local sandbox and physical-device smoke pass is tracked in
[P2.5](docs/task.md#p25-repair-flatpak-behavior-and-local-build-path).

### Connecting to Remote Servers

Remote servers are discovered automatically via mDNS (DAAP, Subsonic, Plex) and UDP broadcast (Jellyfin). Discovered servers appear in the sidebar — click one to connect. Password-protected DAAP shares show a lock icon; passwordless shares connect with a single click.

To manually add a server, click the **+** button in the sidebar toolbar and enter the server type (Subsonic, Jellyfin, or Plex), URL, and credentials. Manually-added servers are persisted across launches (credentials are entered in the UI only — they are not stored on disk).

### Searching Your Library

Use the **search bar** above the browser panes to filter tracks in real-time. The search matches across title, artist, album, and genre simultaneously, and composes with any active browser pane selections. Clear the search by clicking the ✕ button or pressing Escape.

### Editing Song Metadata

Right-click any local track and select **Properties…** to view and edit its metadata. The Properties dialog supports:

- **Single-track editing** — Title, Artist, Album, Genre, Composer, Year, Track #, Disc # (plus read-only Format, Bitrate, Sample Rate, Duration, and File Path)
- **Batch editing** — Select multiple tracks, then right-click → Properties. Only batch-appropriate fields are shown (Artist, Album, Genre, Composer, Year, Disc #). Fields with mixed values display "Mixed" as a placeholder; only fields you explicitly change are written.
- **MusicBrainz Lookup** — In single-track mode, click "MusicBrainz Lookup" to search by title + artist. Results populate the form but are **not** saved automatically — you must click Save.

All edits require an explicit **Save** click. Cancel discards all changes. Numeric edits are
validated before a file is touched. Before editing is enabled, a background capability check
requires every exact selected path to be a supported, readable, non-symlink regular file and
rehearses create, flush, replace, and cleanup using two empty writer-owned siblings once per containing
directory, stopping after the first blocked directory. Malformed or mixed local/remote selections
fail closed instead of silently editing only a subset, and duplicate playlist rows write their
exact file only once. Save rechecks the complete
selection before writing the first track; the result is necessarily point-in-time, so an unplug,
permission change, full filesystem, or target-specific lock can still make the real operation fail
safely. Successful saves use an exclusively created, bounded
`.tributary-tag-<UUID>.<format>` sibling carrying only the case-normalized source format extension,
flush it before atomic replacement, and remove the sibling on success or attempt removal on every
failure path. Unix copies begin private at mode `0600` before receiving the source mode; on Windows,
each file's source DACL is independently installed on an empty sibling and must permit a fresh
read/write/delete handle before any batch write begins. The real copy follows the same exclusive
no-sharing sequence before its first audio byte, so a permissive parent-directory ACL cannot briefly
expose it. Cleanup I/O and process termination remain fallible, so local scans and the filesystem
watcher also recognize and exclude
only that exact internal shape: an in-progress or residual copy never appears as another library
track, and final replacement refreshes the original path without losing its stable identity,
history, or playlist links. Supported formats:
MP3 (ID3v2), M4A/AAC, OGG Vorbis, and FLAC.

### Playlists

Tributary supports regular and smart playlists for the local library:

- **Regular playlists** — Right-click the Playlists header in the sidebar to create a new playlist. Right-click tracks in the tracklist to add them. Playlists survive library folder changes via fingerprint-based track matching.
- **Smart playlists** — iTunes-style rules engine with filterable metadata fields, text/numeric/date operators, sorting, and result limiting. Smart playlists are evaluated against the current local library whenever they are opened or exported; they are not stored snapshots. Create them via the sidebar context menu.

#### Importing and exporting playlists

Tributary directly reads and writes only [XSPF version 1](https://www.xspf.org/spec) (`.xspf`).
The menus and file chooser identify that format explicitly; Apple Music/iTunes XML, Google
Takeout CSV, M3U, and service-specific playlist URLs are not accepted directly. Export writes the
complete XSPF document to a temporary sibling and atomically replaces the chosen destination, so
an error leaves an existing export unchanged. XML 1.0-forbidden control characters are rejected
before the temporary file or destination is touched. A corrupt negative stored duration or one
outside Tributary's supported `u64` millisecond range is omitted rather than blocking the otherwise
valid playlist, because XSPF duration is optional.

Import requires a valid leading XML 1.0 declaration when one is present, `version="1"`, and the
canonical XSPF namespace, expressed either as the default namespace or through a prefix. It
validates every attribute's XML syntax and namespace binding; rejects DTDs, malformed or trailing
documents, and elements that only look like tracks inside comments, CDATA, extensions, or other
nesting; and imports only direct XSPF `<track>` children of `<trackList>`. Standard named and numeric
character references are decoded by the XML parser.

On import, each XSPF `<track>` is resolved against the local library in this order:

1. An exact local path decoded from a valid `file:` URI in `<location>`. HTTP(S), other schemes,
   and malformed locations are ignored as paths, though the row can still match by metadata.
2. An exact title + artist match after trimming whitespace and ignoring case; a supplied album is
   also exact. This is normalization, not fuzzy or “similarly named” matching.
3. If `<duration>` is present, only candidates within five seconds qualify and the unique nearest
   duration wins. Without a duration, the metadata candidate must already be unique. Ties and
duplicate metadata remain unmatched instead of choosing an arbitrary song.

Only a valid imported `<location>` supplies path authority. Metadata-only imports and tracks added
inside Tributary remain fingerprint-only across later relinks, so a different song that eventually
reuses a scanned library path cannot silently take over the playlist entry. Corrupt negative or
out-of-schema library durations are ignored as optional matching evidence rather than wrapped or
allowed to block an otherwise safe path/fingerprint reconciliation.

The entire playlist and its entries commit in one database transaction. The completion dialog
reports **matched**, **unmatched**, and **failed** counts. An unmatched entry with a usable path or
title/artist fingerprint is preserved in playlist order and can be linked by a later library
reconciliation; it is not currently playable until a unique local match appears. A row fails only
when it has no usable path or title/artist identity (or contains a duration too large for the
playlist schema). A database or write failure rolls the import back and does not add a sidebar row.
An XSPF `<duration>` that is not a valid unsigned millisecond value rejects the document before a
database transaction begins; a syntactically valid value that exceeds the playlist database range
instead counts that individual row as failed.

Apple Music and iTunes can export playlist metadata as XML, but their XML is an Apple property-list
format, not XSPF. Follow Apple's official export steps for
[Music on Mac](https://support.apple.com/en-ca/guide/music/-mus27cd5060f/mac) or
[iTunes on Windows](https://support.apple.com/guide/itunes/itns2998/windows), then use a converter
that dereferences each playlist item's `Track ID` through the XML `Tracks` dictionary and emits one
XSPF `<track>` with these mappings:

| Apple XML track key | XSPF v1 element | Conversion |
|---|---|---|
| `Location` | `<location>` | Preserve the `file:` URI when it points at the same local file; otherwise update it to the local library path. |
| `Name` | `<title>` | Copy as text. |
| `Artist` | `<creator>` | Copy as text. |
| `Album` | `<album>` | Copy when present. |
| `Total Time` | `<duration>` | Copy as milliseconds; both formats use milliseconds here. |

Apple's export contains metadata and references, not the audio files themselves. Subscription-only
or unavailable catalog items may therefore lack a corresponding local path, and duplicate editions
with identical normalized metadata stay unmatched unless duration identifies one uniquely.

For YouTube Music, use the official [Google Takeout download
flow](https://support.google.com/accounts/answer/3024190) to obtain your YouTube/YouTube Music data.
Takeout's CSV layout and available fields can vary, so convert each playlist row field by field:

| Takeout value, when available | XSPF v1 element | Conversion |
|---|---|---|
| A verified path to the corresponding local audio file | `<location>` | Encode it as a `file:` URI. Do **not** put a YouTube watch URL here and expect local-file matching. |
| Track/song title | `<title>` | Copy the music title, removing video-only decoration only when you can verify it. |
| Music artist | `<creator>` | Copy the artist; a channel/uploader name is often not the tagged artist and should not be guessed. |
| Release/album | `<album>` | Copy only when Takeout supplies or you can verify it. |
| Duration | `<duration>` | Convert seconds to milliseconds; omit rather than estimate. |

An archive containing only video IDs, watch URLs, or timestamps does not contain enough information
for safe local-library matching; enrich it with verified local metadata before creating XSPF.
Google also documents a [direct Takeout transfer to Apple
Music](https://support.google.com/accounts/answer/14792019), after which the Apple XML conversion
above can be used. That service transfer is a one-time copy, currently requires Apple Music,
transfers all user-created playlists rather than selected ones, may omit songs absent from the
destination catalog, and excludes saved third-party playlists, user-uploaded/private content, and
podcasts. Tributary deliberately does not guess through any of those gaps.

### Playback Controls

- **Play/Pause** — click the circular play button, or double-click any track in the tracklist
- **Next / Previous** — skip buttons; Previous restarts the current track if more than 3 seconds in
- **Shuffle** — randomises track order (avoids repeating the current track)
- **Repeat** — cycles through Off → All → One
- **Seek** — drag the progress scrubber
- **Volume** — drag the volume slider (cubic perceptual curve)

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+F` / `Cmd+F` | Focus search bar |
| `Ctrl+Q` / `Cmd+Q` | Quit |

### Preferences

Open **Preferences** from the hamburger menu (☰) to:
- Change the local music library folders (supports multiple directories)
- Toggle browser filter panes (Genre, Artist, Album)
- Show/hide tracklist columns

---

## AirPlay 2 roadmap

AirPlay 2 receivers (HomePod, recent Apple TVs, AirPlay-2-certified third-party speakers) advertise via `_airplay._tcp.local.` and are detected by the discovery layer today, but the output implementation only speaks the legacy RAOP protocol via GStreamer's `raopsink` element. AirPlay 2 uses a different protocol stack and cannot be driven by `raopsink`, so AirPlay-2-only devices are filtered out of the output selector to avoid silent playback failures. Re-enabling them requires real sender-side support to land first.

Sender-side AirPlay 2 support requires, at minimum:

1. **A pairing/handshake step** to establish an authenticated session with the receiver before any audio is sent.
2. **An encrypted control channel** carrying the post-handshake messaging.
3. **An audio streaming path** delivering encoded audio in the format and timing the receiver expects.
4. **Multi-device clock sync** — only relevant if multi-room playback is in scope.

Each of these has specifics (key exchange algorithms, audio codec, RTSP/HTTP verbs, timing format) that need to be confirmed against current AirPlay 2 reverse-engineering work before any concrete dependency or implementation can be committed. This README intentionally does not enumerate those details — they belong in a design doc once an implementation path is chosen.

Likely paths forward (each to be evaluated when the work begins):

- **Subprocess delegation** to a maintained external tool. Cheaper to integrate, but adds a runtime dependency outside the single-binary distribution model.
- **A pure-Rust sender implementation**, either in-tree or as a contributed `gst-plugins-rs` element, so it can plug into the same pipeline pattern `raopsink` uses today. Higher engineering cost; cleanest distribution story.
- **Wait for an upstream component** to mature to the point that one of the above becomes obviously preferable.

The hook for whichever path is chosen is `service_type: "airplay2"` in [`src/discovery.rs`](src/discovery.rs); today that branch is dropped by [`src/ui/discovery_handler.rs`](src/ui/discovery_handler.rs), and that's where AirPlay 2 sender support will plug in.

---

## License

Tributary is licensed under the [GNU General Public License v3.0 or later](LICENSE).
