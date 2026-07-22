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
| Shuffle & repeat (off / all / one) with bounded actual Previous/forward history and persistence | ✅ |
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
| Regular & smart playlists (iTunes-style rules engine) | ✅ Regular playlists mix local and authenticated Subsonic/Jellyfin/Plex/DAAP entries; smart playlists remain local-library queries ([#142](https://github.com/jm2/tributary/pull/142)) |
| Subsonic server-native playlist import/sync | ✅ Pull-only browser with detached Import Copy, read-only Keep Synced, reconnect/manual refresh, and visible conflict, missing, offline, retry, replace, unlink, and removal recovery ([#149](https://github.com/jm2/tributary/pull/149)) |
| Realtime text search filter (title, artist, album, genre) | ✅ |
| Song metadata editing (Properties dialog with Save/Cancel) | ✅ |
| Batch metadata editing (multi-select) | ✅ |
| MusicBrainz auto-fill lookup | ✅ |
| Keyboard shortcut: `Ctrl+F` / `Cmd+F` to search | ✅ |
| XDG music directory support (non-English locales) | ✅ |
| Network connection guard (prevents duplicate auth) | ✅ |
| i18n/l10n framework (13 languages, auto locale detection) | ✅ |
| Audio output selector (local + MPD + Chromecast; AirPlay discovery seam) | ✅ |
| MPD output backend (sink-only, TCP with security hardening) | ✅ Requires explicit exclusive-control confirmation |
| Output switching (click to swap local ↔ MPD) | ✅ |
| AirPlay 1 (RAOP) output | ⚠️ Discovery and a fail-closed integration seam exist, but current supported GStreamer/Homebrew/MSYS2 packages do not ship the required `raopsink` sender |
| AirPlay 2 / HomeKit output | ❌ Not yet supported — see [AirPlay roadmap](#airplay-roadmap) below |
| Chromecast output (Cast V2 — local files + remote sources) | ✅ |
| Album artist sort (preference toggle) | ✅ |
| Smart playlist compound sort (multi-key ordering) | ✅ |
| Geo-distance sorting for Stations Near Me | ✅ |
| USB/removable-media browsing (live native sidebar + bounded track scan) | ✅ |
| USB file transfer (copy to device with progress) | ❌ Planned ([#8](https://github.com/jm2/tributary/issues/8)) |
| Multiple music library directories | ✅ |
| Playlist import/export (XSPF) | ✅ |
| Rhythmbox profile migration | ✅ Preview-first, exact-path import of ratings, play counts, optional last-played timestamps, and representable playlists ([contract](docs/rhythmbox-migration.md)) |
| Durable local playback history | ✅ Exact accepted local occurrences persist a saturating play count and monotonic last-played timestamp, with live Plays refresh ([contract](docs/playback-history.md)) |
| Default smart playlists (Recently Added, Recently Played, Top 25) | ✅ Recently Played and Top 25 use deterministic authoritative history, safe untouched-default migration, and live projection refresh ([P1.3](docs/task.md#p13--record-trustworthy-local-playback-history)) |
| Track ratings | ✅ Exact 1–100 local editing, read-only/unsupported source states, deterministic sorting, live refresh, and smart-playlist rules ([contract](docs/ratings.md)) |
| Last.fm scrobbling | 🚧 Internal foundation only — bounded protocol/vault storage, a private durable FIFO, a latest-only desktop-authorization owner, frozen generation-owned playback evidence, a GTK-free move-only playback-owner/handoff boundary, registry-instance-bound real-tag external/removable attribution, runtime-owned one-shot now-playing, and a non-recreatable process playback coordinator with production event/terminal/source/output/shutdown ingress are implemented. A sealed headless Active bridge can bind one exact window to one private playback-only runtime capability, create the sole coordinator-minted playback owner, revalidate source policy, and order NowPlaying/Enqueue/Clear admission and retirement. Startup deliberately remains Dormant: no production path starts the Last.fm runtime, claims its playback ingress, issues activation, or wires authorization, settings, and live policy, so no Last.fm work is emitted and the feature remains unavailable to users. Exact local/authenticated-remote profiles, production remote opt-in, consent/browser launch, vault installation and account-transition policy, account/recovery/status UX, localization/accessibility, production credentials/verification, and live final acceptance testing remain ([complete inventory](docs/lastfm-scrobbling.md#dated-implementation-boundary)) |
| Window position persistence | ✅ |
| Windows 11 Snap Layout support | ✅ |
| Linux and macOS file associations | ✅ |
| Cross-platform: Linux, macOS, Windows | ✅ |
| Light & dark mode | ✅ Automatic (libadwaita) |

The internal Last.fm authorization and playback runtimes distinguish joined lifecycle retirement
from a hard task abort. Authorization keeps its one-hour request token and token-bearing browser URL
entirely behind latest-only owner authority, with no production URL accessor or browser handoff yet;
an explicit consent-gated handoff remains product-integration work. Normal lifecycle and
supervised-failure paths cancel and join network work before releasing authority. An external
playback-runtime owner abort marks the drain barrier failed, while the child request keeps a shared
vault lease until its future is actually dropped so a successor cannot overlap it.

The internal playback owner consumes one move-only proof that binds an accepted output generation
to either frozen eligible metadata or an explicit ineligible replacement. In production code, only
`PlaybackSession` can issue the private mint witness after that exact generation crosses output
acceptance, and each `QueueItem` keeps its occurrence metadata frozen. Managed external and
removable proofs are bound to one registry instance, exact session or catalogue authority, and an
exact real-tag profile; policy, profile, epoch/generation, authority, and membership are
revalidated under the lifecycle lock. External and removable title and artist must both come from
parsed tags—filenames and a display-only `Unknown` album never substitute. Removable queue capture
asks the live registry to mint the exact current session reference before freezing the occurrence.
Authenticated remotes remain closed because their exact profiles and production opt-in source set
do not exist yet. Lock-linearized freshness leaves delayed accepted loads and superseded
NowPlaying/Clear handoffs inert, while a qualified Enqueue is not retroactively revoked. Issuing a
successor NowPlaying does not discard a predecessor Clear prematurely: source rejection or failed
runtime admission preserves that Clear, and only successful successor admission retires it.

Startup claims one non-cloneable, non-recreatable process coordinator before GTK activation and
transfers it only to the first window. Its cloneable ingress is exact-window and epoch-bound, so
stale-window callbacks are inert. The sealed headless Active bridge can consume one non-cloneable,
one-shot playback capability already claimed from a running runtime, use a module-private mint to
construct its sole playback owner, and snapshot the enabled authenticated-remote source set.
Accepted loads, playback events and discontinuities, source revalidation, and typed retirement then
produce ordered NowPlaying, Enqueue, and Clear admissions through that restricted runtime
capability. Activation close, window rebind, owner shutdown, and owner drop first revoke admission,
drain in-flight operations, retire the occurrence, and share the exact retirement result before a
successor can become Dormant or Active; owner, gate, closed-runtime, and retirement failures close
the environment terminally rather than admitting a replacement. The lazy accepted-load metadata
extractor runs only inside that drain barrier, outside the coordinator mutex, and must remain
bounded and non-reentrant; a race-losing result is revoked without dispatch.

Production playback already reports output intent before invocation, handles the accepted/rejected
session result, hands accepted loads to the lazy coordinator boundary, and reports current events,
seek/Previous/resume discontinuities, Stop, committed output replacement, queue/terminal
retirement, source-authority revalidation points, and shutdown without carrying GTK borrows across
coordinator ingress. This is still not feature activation: application startup leaves the
coordinator Dormant and does not start a Last.fm runtime, claim its playback ingress, issue an
activation, construct authorization/settings UI, or supply live source policy. Dormant, stale, and
shutdown loads consume and revoke their exact authority through a metadata-free discard closure,
so no external/removable metadata or action handoff leaves the playback session. Exact
local/authenticated-remote profiles and the production activation path remain follow-on work.

See the [implementation roadmap](docs/roadmap.md) for the audited open-issue backlog, proposed
ordering, and explicit current limitations. The countable working list is
[`docs/task.md`](docs/task.md).

### Migrating from Rhythmbox

Choose **Import from Rhythmbox…** from Tributary's application menu, select the Rhythmbox profile
folder containing `rhythmdb.xml` and, when present, `playlists.xml`, and review the preview before
applying it. Ratings and play counts are enabled by default; last-played timestamps and replacing a
different Tributary rating require explicit choices. If the music library moved, the optional root
remap replaces one exact old path prefix with one exact current prefix.

Migration never guesses by title, artist, album, or a similar filename. A source row changes a track
only when its mapped absolute file path exactly matches the current local library. The preview lists
bounded details for unmatched paths, conflicts, invalid source data, skipped queues, and playlist
rules Tributary cannot represent; applying that safe subset requires an explicit acknowledgement.
Static playlists retain source order, duplicates, and valid unmatched occurrences as exact
path-only intent that can reconcile later; automatic playlists are imported only when their
supported play-count/rating rules can be reproduced exactly. One atomic database transaction
applies the accepted preview, and an exact repeat of the same source snapshot and policy is a
no-op. See the [Rhythmbox migration contract](docs/rhythmbox-migration.md) for limits, privacy
boundaries, and the intentionally conservative smart-playlist subset.

### MPD output safety

MPD exposes pause, stop, and its `repeat`, `random`, `single`, and `consume` options as
partition-wide commands; it does not provide an atomic “change this only if Tributary still owns
the current song” operation. Tributary therefore plays through an MPD output only after **Add
Output** confirms that this Tributary instance has exclusive control of that playback partition.
Do not use another MPD controller or another Tributary instance against the same partition while
it is configured as an output. Each load sets the four MPD options above to off, and those daemon
settings can remain changed after playback.

The confirmation is persisted in `outputs.json`. Entries saved by an older Tributary release have
no confirmation and fail closed before optimistic Buffering state, output-epoch advancement,
worker enqueue or cleanup, any MPD connection, playback-state or option command, or protected-media
ticket. The worker independently repeats the gate, and malformed, unsupported, or inactive media
cannot bypass the same confirmation guidance. A refused queue item remains retryable, so another
Play re-shows the guidance rather than toggling an empty MPD session. Re-add the same host and port
and select the exclusive-control checkbox to upgrade that entry in place; Tributary preserves its
existing name and does not add a duplicate. If that legacy output was already selected, select its
row again so the confirmed mode rebuilds the output before playback. If a foreign current song is
nevertheless observed after confirmation, Tributary still relinquishes ownership and
conservatively retains its queued ID rather than risking disruption of the foreign playback.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│ GTK4 / libadwaita UI and platform media controls            │
├──────────────────────────────────────────────────────────────┤
│ SourceRegistry: identity, provenance, lifecycle, epochs      │
│ and playback-time retained media resolution                  │
├──────────────────────────────────────────────────────────────┤
│ MediaBackend catalogue seam                                  │
├────────┬──────────┬──────────┬──────┬──────┬────────┬────────┤
│ Local  │ Subsonic │ Jellyfin │ Plex │ DAAP │ Radio  │ Device │
├────────┴──────────┴──────────┴──────┴──────┴────────┴────────┤
│ AudioOutput: Local/GStreamer │ MPD │ AirPlay 1 │ Chromecast  │
└──────────────────────────────────────────────────────────────┘
```

This is the shipping architecture. Local, Subsonic, Jellyfin, Plex, and DAAP publish complete
catalogues through the shared `MediaBackend` boundary. `SourceRegistry` is the lifecycle and
playback-authority owner for authenticated remotes, Radio-Browser views, removable mounts, and
ephemeral operating-system-opened files. Generic GTK rows and playback queues retain stable
`SourceId`, exact backend-native `TrackId`, and a non-secret publishing epoch rather than a server
address, credential-bearing URL, mount path, or local file locator.

Last.fm is intentionally absent from the shipping diagram because it is not user-visible. Its
sealed internal path is `process coordinator → exact-window activation → coordinator-minted
playback owner → one-shot playback runtime ingress → serialized runtime actor`. The
activation owns a registry and remote-opt-in snapshot for synchronous source admission, and its
operation/retirement barriers prevent a successor environment from overtaking accepted playback
work or the predecessor's final Clear. Application startup currently stops at the coordinator's
Dormant state and never constructs that path.

The local backend's aggregate contract and complete-catalogue integration seam are now stable.
Local artist and album IDs use a private, versioned UUIDv5 namespace with separate artist/album
domains and length-framed exact metadata. An artist key is its stored performing-artist name; an
album key is its stored title plus its effective album artist. A missing or Unicode-whitespace-only
album-artist tag falls back to the performing artist, while every nonblank value is preserved
exactly—case, normalization, and surrounding whitespace included. This keeps same-titled albums by
different artists separate and groups compilation tracks that share an album artist. Local tracks
carry those same aggregate IDs, and `LocalBackend` can resolve album/artist track lists; it resolves
the compact metadata key first, then restricts SQLite to the exact album title or performing artist
instead of loading all track models. Unknown aggregate IDs return an empty list.

Album year and genre are deterministic numeric and lexical minima, respectively, when track tags
disagree, rather than values from an arbitrary SQLite row. `Album.artist_id` deliberately remains
empty because a compilation's album artist is not necessarily any performing-artist entity.
Identity-bearing metadata edits therefore produce a new aggregate ID. Persisted local track IDs
are preserved byte-for-byte; malformed legacy IDs use a frozen deterministic compatibility
projection rather than a random fallback. Local occurrences in local or playlist queues resolve
the exact current database row beneath retained, revalidated root and file authority at use time;
authenticated remote playlist occurrences resolve through their exact current registry guard.

The [source identity and lifecycle decision](docs/architecture/source-lifecycle.md) documents that
implemented seam, including provenance, cancellation, explicit DAAP/Jellyfin session retirement,
and retained at-use authority for every source kind. Product additions and the few explicitly
deferred authority extensions are tracked in the [active backlog](docs/task.md), not in the
historical remediation plan.

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

### Release artifact component policy

Tributary does not play DVDs, Blu-ray discs, or DRM-protected media. Its packaging helpers exclude and
fail closed on dedicated optical-disc copy-control/decryption components, unused disc-access
plugins that can introduce them transitively, and proprietary content-decryption modules. Windows
and macOS validate their self-contained application trees; native Linux packages validate their
own payload, package relationships, and installer metadata, while Flatpak validates its complete
app-owned commit (`/app`, exports, and metadata) rather than the separately delivered shared
runtime. Windows rejects filesystem reparse points before copying and reopens the completed ZIP, while the
same final validation protects incremental and installer-only packaging from stale files.

Ordinary audio codecs, TLS, and general-purpose cryptography are intentionally distinct from that
deny policy and remain available for supported playback. See the
[release component policy](docs/release-component-policy.md) for the exact enforcement and review
boundary.

### Flatpak (Linux)

The manifest builds offline (`CARGO_NET_OFFLINE=true`) from a generated
`build-aux/flatpak/cargo-sources.json`. The repository vendors the immutably pinned Cargo source
generator, and the shared helper verifies its recorded checksum before writing the ignored source
manifest beside the Flatpak manifest. Local builds and CI therefore run the same generator from the
same location.

```bash
# Install the tools and configure Flathub for this user:
sudo apt install binutils flatpak flatpak-builder ostree python3-venv
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
# Run every host target and feature (unit, integration, and proptest suites):
cargo test --all-targets --all-features --locked

# Install the exact compiler, LLVM tools, and coverage frontend used by CI:
rustup toolchain install 1.92.0 --profile minimal --component llvm-tools-preview
cargo +1.92.0 install cargo-llvm-cov --version 0.8.7 --locked

# Run the Linux x86_64 coverage gate and print its summary:
minimum="$(tr -d '[:space:]' < coverage-baseline.txt)"
cargo +1.92.0 llvm-cov clean --workspace
cargo +1.92.0 llvm-cov --all-targets --all-features --locked --summary-only \
  --fail-under-lines "$minimum"

# Or generate the complete HTML report:
cargo +1.92.0 llvm-cov --all-targets --all-features --locked --html \
  --output-dir coverage --fail-under-lines "$minimum"
```

CI's comparable coverage metric is one aggregate Linux x86_64 run pinned to Rust 1.92.0,
`llvm-tools-preview`, cargo-llvm-cov 0.8.7, the committed dependency lockfile, every host target,
and every feature. It does not exclude UI, backend, migration, desktop-integration, or entry-point
files. Every test suite still executes; cargo-llvm-cov's default omission of test-only source files
keeps the percentage a production-code denominator. Other Linux architectures, macOS, and Windows
`--coverage`/`-Coverage` helpers report their native source sets too, but those summaries are
informational because they use the active compiler and conditional code cannot produce the same
percentage as the pinned Linux x86_64 job.

[`coverage-baseline.txt`](coverage-baseline.txt) is the minimum accepted line percentage. To raise
it, run the canonical clean Linux command twice, take the lower total, round down to one decimal,
and subtract 0.1 percentage point for instrumentation noise. CI enforces the checked-in value but
does not compare it with the base branch. The repository review policy treats the floor as a
ratchet: ordinary changes keep or raise it, while lowering it requires a dedicated
measurement-definition change that explains the source-set or tooling change and records a new
two-run baseline. This is not a claim that every platform branch is exercised by one host.

CI automatically runs on every push/PR:
- **Security audit** — `cargo audit` checks dependencies against the RustSec Advisory Database
- **Pedantic Clippy** — `clippy::pedantic` + `clippy::nursery` with `-D warnings`
- **Code coverage** — pinned, comprehensive `cargo-llvm-cov` Linux x86_64 line-floor gate governed
  by the repository review ratchet, plus an HTML report uploaded even when the threshold fails
- **Weekly fuzzing** — `cargo-fuzz` target for the DMAP binary parser (5 min, Sundays)

---

## Project Structure

```
src/
├── main.rs                 # Application entry point (GTK + tokio bootstrap)
├── panic_reporting.rs      # Process-wide content-free panic diagnostics
├── discovery.rs            # mDNS + UDP zero-config server discovery
├── architecture/
│   ├── mod.rs              # Module root & re-exports
│   ├── models.rs           # Track, Album, Artist, SearchResults, LibraryStats
│   ├── backend.rs          # MediaBackend async trait
│   ├── identity.rs         # Stable source/media/view identity types
│   ├── media.rs            # Retained resolved-media capabilities
│   └── error.rs            # BackendError (thiserror)
├── source_registry.rs      # Source lifecycle, provenance, playlist authority, at-use resolution
├── server_playlist_coordinator.rs # GTK-free latest-request lanes, admission, and shutdown drain
├── removable.rs            # Retained removable-mount catalogue/media adapter
├── external_file.rs        # Ephemeral retained OS-open file adapter
├── audio/
│   ├── mod.rs              # GStreamer Player (playbin3, bus watch, position timer)
│   ├── output.rs           # AudioOutput trait abstraction
│   ├── local_output.rs     # Local GStreamer playback (AudioOutput impl)
│   ├── mpd_output.rs       # MPD TCP output (AudioOutput impl)
│   ├── airplay_output.rs   # Runtime-gated AirPlay 1/RAOP sender seam
│   ├── chromecast_output.rs# Chromecast/Cast V2 output (local + remote)
│   └── cast_http_server.rs # Embedded LAN-only HTTP server for Chromecast
├── db/
│   ├── mod.rs              # Database layer root
│   ├── connection.rs       # SQLite init, XDG paths, migration runner
│   ├── entities/
│   │   └── track.rs        # SeaORM entity for tracks table
│   └── migration/
│       └── *.rs            # Ordered, retry-safe SQLite schema migrations
├── desktop_integration/
│   └── mod.rs              # OS media controls via souvlaki (MPRIS/SMTC/Now Playing)
├── local/
│   ├── mod.rs              # Local backend root
│   ├── backend.rs          # MediaBackend impl (LocalBackend)
│   ├── engine.rs           # Async scan + notify FS watcher + LibraryEvent channel
│   ├── server_playlist_browser.rs # Opaque session/action-token broker for GTK browsing
│   ├── server_playlist_runtime.rs # Exact-session reconnect and manual pull facade
│   ├── playback_history.rs # Pure counted-play occurrence accounting
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
├── lastfm/
│   ├── mod.rs              # Private Last.fm integration boundary
│   ├── authorization.rs    # Latest-only desktop authorization owner
│   ├── client.rs           # Bounded signed Last.fm 2.0 protocol client
│   ├── credentials.rs      # Native-vault session and account binding
│   ├── storage.rs          # Private durable FIFO and opaque receipts
│   ├── playback.rs         # Frozen generation-owned occurrence evidence
│   ├── playback_owner.rs   # Coordinator-minted accepted-output owner + ordered handoffs
│   ├── playback_coordinator.rs # Process owner + sealed exact-window runtime bridge
│   ├── delivery.rs         # Exhaustive outcome and retry policy
│   ├── worker.rs           # Single-flight FIFO delivery worker
│   ├── lifecycle.rs        # Shared vault lease and explicit recovery
│   └── runtime.rs          # Serialized actor + one-shot playback-only ingress
├── device/
│   ├── mod.rs              # DeviceInfo model for mounted browsable media
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
    ├── removable_media.rs  # Native mount monitoring + SourceRegistry reconciliation
    ├── discovery_handler.rs# mDNS/DNS-SD event handler (sidebar + output list)
    ├── context_menu.rs     # Tracklist right-click menu (playlist ops + properties)
    ├── playlist_actions.rs # Playlist CRUD (create, rename, delete, reorder)
    ├── server_playlists.rs  # Virtualized Import Copy/Keep Synced browser
    ├── server_playlist_recovery.rs # Linked-mirror status and recovery controls
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
private native mount path: mount UUID is preferred, then volume UUID, Unix device identifier, and
finally root URI. Each eligible key maps deterministically to one `SourceId`. Mount arrival claims
that identity in `SourceRegistry` and automatically begins one bounded, cancellable connection;
selecting the row only displays the accepted catalogue and does not launch a second scanner.

Construction runs on Tokio's blocking pool beneath retained authority for the exact observed mount.
It follows neither directory nor file links, stays on the same filesystem, orders candidates
deterministically, checks cancellation cooperatively, bounds tag metadata, and parses through exact
already-open file handles. The accepted catalogue, GTK rows, caches, playback queue, and artwork
requests contain only `SourceId`, a losslessly encoded mount-relative `TrackId`, metadata, and the
publishing epoch—never an absolute mount path or `file://` locator.

Playback and embedded artwork resolve an exact accepted ID at use time. The adapter revalidates the
live epoch and lease, retained mount, ancestor namespace, containment, regular-file type, and exact
file before returning one retained capability. Replacing a pathname therefore cannot retarget
already admitted playback. Relocation disconnects the old adapter before reconnecting fresh
inventory under a new epoch. Pre-unmount revokes scanning and file authority before UI/playback
cleanup; if the unmount fails, fresh inventory may reconnect. Confirmed removal retires the source
and releases its provenance claim before removing the row, so rapid same-path reattachment cannot
revive stale state.

The key is best-available logical identity, not proof of unique physical hardware: cloned
filesystems may share a UUID, Unix-device and root-URI fallbacks can change with device/path
assignment, and GIO's broad `can_unmount` signal can include a non-removable or native-path network
mount when the backend supplies no class. Tributary does not mount unmounted volumes, eject
devices, cross nested filesystems, or browse MTP-only devices. Pathless removable rows deliberately
omit Properties until a retained mutation authority exists. Real USB add/change/unplug behavior
still needs cross-platform hardware validation. In Flatpak, the inventory can still list an eligible native mount elsewhere,
but automatic Devices file access is read-only and limited to `/media`, `/run/media`, and `/mnt`;
an inaccessible listed root cannot be scanned or played. Writable custom libraries require
explicit selection through the folder portal as described in [Flatpak (Linux)](#flatpak-linux).
The remaining physical-device and installed-sandbox smoke checks are preserved in the archived
[P2.4](docs/task-remediation-2026-07.md#p24-make-removable-media-browsing-safe-and-asynchronous) and
[P2.5](docs/task-remediation-2026-07.md#p25-repair-flatpak-behavior-and-local-build-path) records;
they are validation work unless a real test exposes another defect.

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

### Track ratings

The Rating column shows Tributary's exact whole-number 1–100 value. Activate a local row's rating
button with a pointer or keyboard to open its editor, choose a value, and select **Apply**; select
**Clear** to return it to Unrated. A requested change is shown only after it commits to the local
library. If storage fails, Tributary keeps the old value and shows a localized failure message.
The Rating column can be hidden or reordered in Preferences like the other track columns.

Authenticated Subsonic, Jellyfin, and Plex libraries can publish ratings, but Tributary does not
write them back: those cells say that the exact value or Unrated state is read-only. DAAP and
removable-media rows show Unavailable. Externally opened files have no library row and their
hidden source remains rating-incapable. Radio-Browser's compact station view omits Rating together
with other track-only metadata; its rows remain rating-incapable. Sorting by Rating keeps rated rows
first whether ascending or descending; missing readable values follow them and unsupported rows
come last, with deterministic stable-ID ties.

Smart-playlist rules can compare Rating with **is**, **is not**, **greater than**, **less than**, or
an inclusive **in range** value, and can test **is rated** or **is unrated** explicitly. Numeric
operands must be in 1–100. An unrated readable track does not satisfy a numeric rule (including
**is not**); an unsupported track satisfies neither numeric nor rated/unrated rules. Rating is also
available for compound sorting and for **Highest Rated** or **Lowest Rated** limit selection, with
missing values kept last. Invalid or reversed numeric input is not clamped: the editor retains the
text, explains the problem visibly and accessibly, and disables OK until it is valid. See the
[rating contract](docs/ratings.md) for source conversions, persistence, and playlist-interchange
boundaries.

### Playlists

Tributary supports mixed-source regular playlists and local-library smart playlists:

- **Regular playlists** — Right-click the Playlists header in the sidebar to create one, then
  right-click selected tracks to add them. Every ordered occurrence has its own durable entry ID
  and exact `(SourceId, TrackId)` media identity, so duplicates remain distinct. Local entries
  survive library-folder changes through the existing exact reconciliation contract; current
  authenticated Subsonic, Jellyfin, Plex, and DAAP entries use live catalogue authority without
  persisting a locator, credential, lease, route, session epoch, or display snapshot.
- **Smart playlists** — iTunes-style rules engine with filterable metadata fields, text/numeric/date operators, sorting, and result limiting. Smart playlists are evaluated against the current local library whenever they are opened or exported; they are not stored snapshots. Create them via the sidebar context menu.

**Add to Playlist** accepts built-in local tracks and exact current rows from retained authenticated
Subsonic, Jellyfin, Plex, and DAAP catalogues. The registry first resolves every selected remote
identity against the same current source session and accepted catalogue generation. After the
database transaction stages the complete ordered selection, it repeats that exact validation and
acquires session/catalogue permits immediately before commit. Authority made stale during staging
rolls back every insert; after admission, lifecycle invalidation waits for commit or rollback. The
commit and permits have an independent completion owner, so cancellation cannot strand authority
or abandon commit completion. If one member is unsupported, disconnected, in an invalid
catalogue, or missing, Tributary explains the result in the user's language and adds nothing.
Radio-Browser, removable media, ephemeral external files, and unknown sources remain unsupported.
Removing rows is likewise one atomic operation over their exact durable entry IDs, so a duplicate
or unavailable occurrence can be removed without affecting its neighbors.

Opening a regular playlist retains every stored occurrence in position order. Available local rows
use current database metadata; available authenticated rows use only the registry's sanitized live
metadata. A disconnected or retired source, unsupported owner, invalid catalogue, missing native
track, or missing/unmatched local identity produces an explicit localized unavailable row that
remains visible and removable. Tributary does not display persisted reconciliation fingerprints as
stale metadata or use them to guess another remote track. Stale projection work and results are
discarded; the affected playlist is invalidated and projected again from current authority.
Reconnection restores a row only when the same `SourceId` publishes the same exact native `TrackId`
again.

Inside `SourceRegistry`, regular-playlist catalogue authority defaults to Unsupported. Only the
retained authenticated Subsonic, Jellyfin, Plex, and DAAP adapters explicitly advertise
source-scoped entries; Radio-Browser, removable media, ephemeral external files, and unknown
adapters remain unsupported. Playlist Add and rendering consume an ordered lookup that accepts
only the exact current source, session epoch, and accepted catalogue generation and returns a
dedicated metadata whitelist without paths, URLs, locators, credentials, leases, routes, or raw
backend errors. Its
closed guard carries the non-secret epoch and generation transiently; neither is written to the
playlist. Guarded media maps backend failures to fixed categories and carries a lifecycle-owned
generation lease that is explicitly revoked even if an observer still holds an old snapshot clone.
Malformed or duplicate catalogue-native IDs make that catalogue's playlist-authority index
`Invalid`, while the catalogue can remain available to existing non-playlist UI and repeated
requested IDs remain repeated ordered occurrences. A missing exact track, unavailable session, or
unsupported source receives an explicit unavailable result without erasing valid neighbors. Stream
and artwork resolutions revalidate the transient guard around asynchronous adapter work so
replacement, refresh, disconnect, shutdown, or final source release cannot revive an old result.

Each playlist queue item keeps the playlist as its view origin while taking media ownership from
that row's real source. Local items retain exact file authority; remote stream and artwork requests
must pass guarded at-use resolution again. A refresh, replacement, retirement, disconnect, or
shutdown therefore denies a stale item instead of replaying a cached URL. Only an exact local
occurrence can update Tributary's local playback history. Remote ratings keep their live
read-only/unsupported capability and never gain mutation authority from playlist membership.

Smart playlists and XSPF import/export remain local-only. Mixed-source metadata export requires a
separate no-locator policy. Subsonic server-native playlist integration now has a pull-only
[accepted contract](docs/subsonic-playlist-sync.md): **Import Copy** is defined to create a detached
editable snapshot, while **Keep Synced** is defined to create an opt-in read-only server-
authoritative mirror. The internal foundation can read bounded `getPlaylists` and `getPlaylist`
responses only through the
exact current authenticated Subsonic session; order and duplicates are preserved, stale sessions
are rejected, and playlist membership grants no playback authority. Migration 14 and the internal
playlist manager now support detached atomic imports plus unique read-only pull mirrors with exact
order/duplicates, local-drift conflicts, revision-CAS stale-result rejection, complete-list missing
state, Replace, Unlink, and explicit local removal. Exact-session commit permits are sealed to the
specific pull or absence result being committed, closing the gap between fetching and committing
without persisting live authority or allowing authority from another current operation to be
substituted.

The UI groundwork reads playlist and link state together, gives pull mirrors an explicit typed
read-only/conflict/missing identity, and excludes them from every ordinary playlist edit menu and
track mutation target without depending on their translated name or compatibility backend string.
Ordinary playlist sidebar changes appear only after database commit, including atomic
smart-playlist creation. Migration 15 and a lifecycle-owned publisher give scan seeding, ordinary
CRUD, raw/cascade writes to the two domain tables, and server-link changes one durable revisioned
full-sidebar lane. It reads each revision and complete redacted join coherently, coalesces refresh
hints, polls the local revision for lost hints, and makes GTK ignore equal or older delivery instead
of applying partial row callbacks.

Server-playlist work now runs through a GTK-free latest-request coordinator with distinct typed,
content-redacted lanes for a source, a source/native remote playlist, and a durable local playlist.
A same-key successor cancels only work that has not reached final admission. Once admitted, the
successor waits for both the predecessor task and its move-only guard to settle; unrelated keys
remain concurrent. Reconnect reserves one coordinator-global request stamp before discovery and
shares it across the delayed local fan-out. Direct requests reserve and enqueue atomically against
that stamped submission path, so a manual request begun later wins for an overlapping mirror.

The lifecycle observer schedules one reconnect sweep for each accepted source session epoch and no
more for catalogue-only invalidations in that session. A sweep with no linked mirror returns before
server I/O. Otherwise it captures the exact durable revision of every linked mirror before network
I/O, obtains one complete listing from that exact observed session, uses indexed exact presence or
sealed absence, and admits no more than eight local fan-out operations at once. It neither polls a
server periodically nor infers deletion from an incomplete, failed, stale, or wrong-session
response. Manual Sync Now, Retry, Replace Local with Server,
Unlink, and Remove Local Copy use the same local lane through a redacted internal completion facade;
never-started displacement is distinct from unexpected interruption after start.

Pull and missing-state persistence stages SQL first, then jointly acquires the coordinator admission
guard and the registry authority sealed to that exact pull or absence result. Both survive through
commit or rollback; local Unlink and Remove are coordinator-guarded after staging as well. Normal
shutdown closes coordinator admission before source shutdown, cancels only pre-admission work, and
uses a persistent barrier to drain admitted tasks and guards. Committed changes feed the durable
full-snapshot publisher.

The completed workflow has two visible surfaces:

- **Browse and import:** the Playlists header's **Server Playlists…** action opens a virtualized
  browser over current pull-snapshot sources—currently authenticated Subsonic. The source picker
  and Reload action publish bounded name/owner hints while the headless broker retains the exact
  source session and native playlist identity. GTK receives only opaque session and one-shot row
  tokens. **Import Copy** creates a detached editable playlist; **Keep Synced** creates a read-only
  pull mirror. Capacity rejection remains retryable, every non-Busy settlement consumes its row
  token, and reload/source/runtime replacement, close, shutdown, or stale delivery revokes the
  applicable session. Checked generations keep older results from changing current widgets.
- **Inspect and recover:** selecting a linked mirror exposes its localized, accessible status and
  only the actions valid for its state and exact-session availability: **Sync Now**, **Retry**,
  **Replace Local with Server**, **Unlink**, and **Remove Local Copy**. Network recovery is disabled
  while its source is unavailable; Unlink and Remove remain source-independent. Replace, Unlink,
  and Remove require confirmation. Targetless actions re-read the selected typed mirror and
  generation before dispatch, disable conflicting controls during work, and ignore stale inspection
  or completion results. Reconnect and durable full-sidebar publication refresh the same state
  without partial row mutation.

This completed P1.5 scope remains deliberately pull-only: it performs no server playlist
creation/update/deletion, fuzzy metadata merge, mixed-source XSPF export, or periodic server
polling. Only authenticated Subsonic has server-playlist read authority, and server-native playlist
IDs, source-session receipts, credentials, routes, and raw backend content never enter GTK action
targets, properties, or diagnostics. See
[P1.5](docs/task.md#p15--persist-source-scoped-playlists), the
[source-scoped regular-playlist contract](docs/source-scoped-playlists.md), the
[Subsonic pull-sync contract](docs/subsonic-playlist-sync.md), and
[#149](https://github.com/jm2/tributary/pull/149) for the completed implementation.

#### Importing and exporting playlists

Tributary directly reads and writes only [XSPF version 1](https://www.xspf.org/spec) (`.xspf`).
The menus and file chooser identify that format explicitly; Apple Music/iTunes XML, Google
Takeout CSV, M3U, and service-specific playlist URLs are not accepted directly. Export writes the
complete XSPF document to a temporary sibling and atomically replaces the chosen destination, so
an error leaves an existing export unchanged. XML 1.0-forbidden control characters are rejected
before the temporary file or destination is touched. A corrupt negative stored duration or one
outside Tributary's supported `u64` millisecond range is omitted rather than blocking the otherwise
valid playlist, because XSPF duration is optional.

XSPF currently represents only local-library tracks. Exporting a regular playlist that contains
any remote or unresolved occurrence is therefore refused as a whole with a visible explanation
before the destination is touched; Tributary never silently exports just the playlist's local
subset. Mixed-source metadata export remains deferred until it has an explicit no-locator policy.

Ratings are deliberately outside this playlist interchange. XSPF v1 has no standard rating field,
so export emits none; import treats rating-like `<meta>` and extension content as inert and never
changes a matched local track's app-owned rating. See the [rating contract](docs/ratings.md) for the
ownership and separate opt-in metadata-transfer boundary.

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

XSPF import continues to create entries owned by the built-in local source. The source-scoped
storage migration does not make an HTTP(S) location or service identifier a remote playlist
authority. Export refuses a regular playlist containing any remote or unresolved occurrence before
touching the destination; it never emits a truncated local-only subset. Mixed-source metadata export
is explicitly deferred until it has a policy that cannot request or serialize a protected remote
locator.

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
- **Next / Previous** — skip buttons and OS media controls share the same behavior. More than three
  seconds into a track, Previous first restarts it; otherwise it walks the actual prior queue
  occurrence. After walking backward, Next replays that fixed forward history before randomizing.
- **Shuffle** — randomises complete queue-occurrence cycles without an immediate rollover repeat.
  Tributary retains the current occurrence plus ten real predecessors; the oldest retained boundary
  restarts instead of inventing a random predecessor. Toggling shuffle starts a fresh traversal at
  the unchanged current track.
- **Repeat** — cycles through Off → All → One
- **Seek** — drag the progress scrubber
- **Volume** — drag the volume slider (cubic perceptual curve)

The [local playback-history contract](docs/playback-history.md) defines a counted play as half of a
known duration, rounded up and capped at four minutes, with a conservative unknown-duration rule.
Production playback now persists each qualifying exact local queue occurrence—including a local
regular/smart-playlist projection—at most once. Rejected or stale loads earn nothing; pause,
buffering, retry, seek, and the Previous restart re-anchor the same occurrence, while paused polls
stay inert until Playing and real navigation or Repeat One creates a new occurrence. Current output
replacement ends playback. The database update targets the stable local track ID atomically,
repairs a legacy-negative count, saturates its count, keeps the newest
trustworthy timestamp, and refreshes the Plays row and
playlist projections only after commit. Normal shutdown first closes the shared GTK command gate,
disables playback/media/open-file producers, and appends a FIFO marker, so no later callback can
queue behind the admitted history/root-trust commands it waits to finish. The disabled window can
remain visible while an earlier serialized library scan finishes.
The gated AirPlay 1 seam contributes the same evidence through generation-scoped 500 ms position
updates only when an external compatible sender is present. Remote, radio, removable, and ephemeral
files do not write local history. Recently Played now uses one
evaluation clock and includes only valid, non-future last-played instants in the inclusive previous
14 days, newest first with stable track-ID ties; null or corrupt history yields an intentional empty
playlist. Top 25 admits positive counts—including legacy counts with no timestamp—then uses count
descending, last-played descending with unknown values last, and stable track ID before its 25-item
cap. A committed history event invalidates cached playlist projections, rejects older asynchronous
results, and reloads the active playlist. Fresh installations receive those exact rules, while an
atomic migration upgrades only byte-exact untouched v0.5.0 and successor defaults and preserves
renamed, edited, reformatted, or otherwise divergent playlists. The editor also exposes Last Played
rules/sorts and Most/Least Recently Played limits without collapsing Weeks or Months back to Days.

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

## AirPlay roadmap

Legacy RAOP receivers are discovered today, but Tributary's AirPlay 1 path is only a runtime-gated
integration seam for a GStreamer element named `raopsink`. Current official GStreamer,
Homebrew, and MSYS2 packages do not ship that element, so supported builds report AirPlay 1 as
unavailable instead of recommending an unrelated package. AirPlay 2 receivers (HomePod, recent
Apple TVs, and AirPlay-2-certified third-party speakers) advertise via `_airplay._tcp.local.` and
are also detected by discovery, but remain filtered out because AirPlay 2 needs a different sender
protocol stack. Both paths require a maintained sender implementation and real-device validation.

Sender-side AirPlay 2 support requires, at minimum:

1. **A pairing/handshake step** to establish an authenticated session with the receiver before any audio is sent.
2. **An encrypted control channel** carrying the post-handshake messaging.
3. **An audio streaming path** delivering encoded audio in the format and timing the receiver expects.
4. **Multi-device clock sync** — only relevant if multi-room playback is in scope.

Each of these has specifics (key exchange algorithms, audio codec, RTSP/HTTP verbs, timing format) that need to be confirmed against current AirPlay 2 reverse-engineering work before any concrete dependency or implementation can be committed. This README intentionally does not enumerate those details — they belong in a design doc once an implementation path is chosen.

Likely paths forward (each to be evaluated when the work begins):

- **Subprocess delegation** to a maintained external tool. Cheaper to integrate, but adds a runtime dependency outside the single-binary distribution model.
- **A pure-Rust sender implementation**, either in-tree or as a contributed `gst-plugins-rs`
  element. Higher engineering cost; cleanest distribution and provenance story.
- **Wait for an upstream component** to mature to the point that one of the above becomes obviously preferable.

The hook for whichever path is chosen is `service_type: "airplay2"` in [`src/discovery.rs`](src/discovery.rs); today that branch is dropped by [`src/ui/discovery_handler.rs`](src/ui/discovery_handler.rs), and that's where AirPlay 2 sender support will plug in.

---

## License

Tributary is licensed under the [GNU General Public License v3.0 or later](LICENSE).
