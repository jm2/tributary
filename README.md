<img src="data/tributary.png" width="96" alt="Tributary icon">

# Tributary

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![CI](https://github.com/jm2/tributary/actions/workflows/ci.yml/badge.svg)](https://github.com/jm2/tributary/actions)

A high-performance, **Rhythmbox-style** media manager written in pure Rust with **GTK4** and **libadwaita**.

Tributary provides a unified interface for managing and streaming music from multiple sources — local files, Subsonic/Navidrome, Jellyfin, Plex, and DAAP/iTunes shares — all through a single, responsive library view.

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
| Subsonic / Navidrome backend | ✅ |
| Jellyfin backend | ✅ |
| Plex backend | ✅ |
| DAAP / iTunes Sharing backend (DMAP binary protocol) | ✅ |
| mDNS zero-config discovery (Subsonic, Plex, DAAP) | ✅ |
| Jellyfin UDP broadcast discovery | ✅ |
| DAAP sidebar eject button (disconnect) | ✅ |
| Password-only auth dialog (DAAP) | ✅ |
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

All backends implement a single `MediaBackend` async trait, so the UI layer never knows or cares where the music comes from.

---

## Building from Source

### Prerequisites (all platforms)

- [Rust 1.80+](https://rustup.rs) (stable toolchain)
- `pkg-config`

### Linux

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

Requires [MSYS2](https://www.msys2.org) with the UCRT64 environment:

```powershell
# In an MSYS2 UCRT64 shell:
pacman -S mingw-w64-ucrt-x86_64-gtk4 \
          mingw-w64-ucrt-x86_64-libadwaita \
          mingw-w64-ucrt-x86_64-gstreamer \
          mingw-w64-ucrt-x86_64-gst-plugins-good \
          mingw-w64-ucrt-x86_64-gst-plugins-bad \
          mingw-w64-ucrt-x86_64-gst-libav \
          mingw-w64-ucrt-x86_64-pkg-config \
          mingw-w64-ucrt-x86_64-toolchain
```

Then, in PowerShell:
```powershell
# Ensure Rust's GNU target is installed:
rustup target add x86_64-pc-windows-gnu

# Build and bundle DLLs:
.\scripts\build-windows.ps1
```

This produces `dist/tributary-windows.zip` with the executable and all required DLLs/resources.

### Flatpak (Linux)

```bash
# Install flatpak-builder if needed:
sudo apt install flatpak-builder

# Build and install locally:
flatpak-builder --force-clean --repo=repo --install --user \
  build-dir build-aux/flatpak/io.github.tributary.Tributary.yml
```

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

### Remote Backend Configuration

Remote backends can be configured via environment variables or discovered automatically via mDNS/UDP.

**Subsonic / Navidrome:**
```bash
SUBSONIC_URL=https://music.example.com SUBSONIC_USER=admin SUBSONIC_PASS=secret ./target/release/tributary
```

**Jellyfin:**
```bash
JELLYFIN_URL=https://jellyfin.example.com JELLYFIN_API_KEY=your-api-key JELLYFIN_USER_ID=your-user-id ./target/release/tributary
```

**Plex:**
```bash
PLEX_URL=https://plex.example.com:32400 PLEX_TOKEN=your-plex-token ./target/release/tributary
```

**DAAP (iTunes Sharing):**
```bash
DAAP_URL=http://192.168.1.50:3689 ./target/release/tributary
# With password:
DAAP_URL=http://192.168.1.50:3689 DAAP_PASSWORD=secret ./target/release/tributary
```

**Auto-discovery:** Subsonic, Plex, and DAAP servers are automatically discovered via mDNS (`_subsonic._tcp.local.`, `_plexmediasvr._tcp.local.`, `_daap._tcp.local.`). Jellyfin servers are discovered via UDP broadcast. Discovered servers appear in the sidebar and can be connected with a single click.

---

## Development

### Git Hooks

Tributary includes a pre-commit hook that runs `cargo fmt --check` to prevent formatting errors from being committed. To enable it after cloning:

```bash
git config core.hooksPath hooks
```

### Windows Helper Scripts

On Windows, the build script can also be used for quick formatting and type-checking from PowerShell:

```powershell
# Format code:
.\scripts\build-windows.ps1 -Fmt

# Type-check without a full build:
.\scripts\build-windows.ps1 -Check
```

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
│   └── mod.rs              # GStreamer Player (playbin3, bus watch, position timer)
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
│   └── tag_parser.rs       # lofty audio tag extraction
├── subsonic/
│   ├── mod.rs              # Subsonic backend root
│   ├── api.rs              # JSON response types (Subsonic REST API)
│   ├── client.rs           # HTTP client (MD5 token auth, request building)
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
├── platform/
│   └── mod.rs              # OS-specific abstractions
└── ui/
    ├── mod.rs              # UI module root
    ├── window.rs           # Main window + backend integration bridge
    ├── header_bar.rs       # Playback controls, now-playing, progress, volume
    ├── sidebar.rs          # Source list (local + remote + discovered + eject)
    ├── browser.rs          # Genre → Artist → Album filter panes
    ├── tracklist.rs        # GtkColumnView track listing
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

### Connecting to Remote Servers

Remote servers are discovered automatically via mDNS (DAAP, Subsonic, Plex) and UDP broadcast (Jellyfin). Discovered servers appear in the sidebar — click one to connect. Password-protected DAAP shares show a lock icon; passwordless shares connect with a single click.

You can also configure servers via environment variables (see [Remote Backend Configuration](#remote-backend-configuration) above).

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
| `Ctrl+Q` / `Cmd+Q` | Quit |

### Preferences

Open **Preferences** from the hamburger menu (☰) to:
- Change the local music library folder
- Toggle browser filter panes (Genre, Artist, Album)
- Show/hide tracklist columns

---

## License

Tributary is licensed under the [GNU General Public License v3.0 or later](LICENSE).
