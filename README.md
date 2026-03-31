# Tributary

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![CI](https://github.com/jm2/tributary/actions/workflows/ci.yml/badge.svg)](https://github.com/jm2/tributary/actions)

A high-performance, **Rhythmbox-style** media manager written in pure Rust with **GTK4** and **libadwaita**.

Tributary provides a unified interface for managing and streaming music from multiple sources — local files, Subsonic/Navidrome, Jellyfin/Plex, and DAAP/iTunes shares — all through a single, responsive library view.

## Features (Roadmap)

| Feature | Status |
|---------|--------|
| GTK4 / libadwaita UI (Rhythmbox-style `GtkColumnView`) | ✅ Phase 2 |
| Browser filtering (Genre → Artist → Album) | ✅ Phase 2 |
| Local library with FS `date_modified` scanning | ✅ Phase 3 |
| Real-time filesystem watching (`notify`) | ✅ Phase 3 |
| SQLite persistence (`SeaORM`) | ✅ Phase 3 |
| GStreamer audio playback (`playbin3`) | ✅ Phase 4 |
| MPRIS / SMTC / macOS Now Playing integration (`souvlaki`) | ✅ Phase 4 |
| Playback controls (play/pause, next/prev, seek, volume) | ✅ Phase 4 |
| Auto-advance with repeat-all support | ✅ Phase 4 |
| Subsonic / Navidrome backend | 📋 Phase 5 |
| Jellyfin / Plex backend | 📋 Phase 5 |
| DAAP / mDNS backend | 📋 Phase 5 |
| Cross-platform: Linux, macOS, Windows | ✅ CI scaffolded |
| Light & dark mode | ✅ Automatic (libadwaita) |

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                   GTK4 / libadwaita UI              │
│              (GtkColumnView, HeaderBar)             │
├─────────────────────────────────────────────────────┤
│              MediaBackend trait (async)             │
├──────────┬──────────┬───────────┬───────────────────┤
│  Local   │ Subsonic │  Jellyfin │  DAAP / mDNS      │
│ (SQLite) │ (REST)   │  (REST)   │  (binary proto)   │
├──────────┴──────────┴───────────┴───────────────────┤
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
brew install gtk4 libadwaita pkg-config
cargo build --release
```

To create a `.app` bundle and `.dmg`:
```bash
brew install create-dmg   # optional, for DMG packaging
./scripts/build-macos.sh --dmg
```

The app bundle is at `dist/Tributary.app`, and the DMG at `dist/Tributary.dmg`.

> **Note:** The `.app` bundle includes rpath-fixed dylibs so it can run without Homebrew on the target machine. Code signing and notarization are not yet automated.

### Windows

Requires [MSYS2](https://www.msys2.org) with the UCRT64 environment:

```powershell
# In an MSYS2 UCRT64 shell:
pacman -S mingw-w64-ucrt-x86_64-gtk4 \
          mingw-w64-ucrt-x86_64-libadwaita \
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

---

## Project Structure

```
src/
├── architecture/
│   ├── mod.rs          # Module root & re-exports
│   ├── models.rs       # Track, Album, Artist, SearchResults, etc.
│   ├── backend.rs      # MediaBackend async trait
│   └── error.rs        # BackendError (thiserror)
├── audio/
│   └── mod.rs          # GStreamer Player (playbin3, bus watch, position timer)
├── db/
│   ├── mod.rs          # Database layer root
│   ├── connection.rs   # SQLite init, XDG paths, migration runner
│   ├── entities/
│   │   └── track.rs    # SeaORM entity for tracks table
│   └── migration/
│       └── m20250101_000001_create_tables.rs
├── desktop_integration/
│   └── mod.rs          # OS media controls via souvlaki (MPRIS/SMTC/Now Playing)
├── local/
│   ├── mod.rs          # Local backend root
│   ├── backend.rs      # MediaBackend impl (LocalBackend)
│   ├── engine.rs       # Async scan + notify FS watcher
│   └── tag_parser.rs   # lofty audio tag extraction
├── platform/
│   └── mod.rs          # OS media controls abstraction (stubs, Phase 1)
├── ui/
│   ├── mod.rs          # UI module root
│   ├── window.rs       # Main window + integration bridge
│   └── header_bar.rs   # Playback controls, now-playing, progress, volume
└── main.rs             # Application entry point

scripts/
├── build-linux.sh      # Linux build helper
├── build-macos.sh      # macOS .app/.dmg builder
└── build-windows.ps1   # Windows DLL bundler

build-aux/flatpak/      # Flatpak manifest
data/                    # .desktop & AppStream metainfo
```

---

## Development Phases

1. **Phase 1:** ✅ Project skeleton, core traits, GTK4 window scaffold, CI/CD
2. **Phase 2:** ✅ Full Rhythmbox-style UI with `GtkColumnView`, browser filtering, multi-pane layout
3. **Phase 3:** ✅ Local backend — SQLite (`SeaORM`), `lofty` tag parsing, `notify` FS watching, async engine
4. **Phase 4:** ✅ GStreamer audio playback (`playbin3`), MPRIS/SMTC/macOS Now Playing (`souvlaki`), full transport controls
5. **Phase 5:** Remote backends (Subsonic, Jellyfin, DAAP)

---

## License

Tributary is licensed under the [GNU General Public License v3.0 or later](LICENSE).
