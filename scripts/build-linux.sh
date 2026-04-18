#!/usr/bin/env bash
# scripts/build-linux.sh
# Tributary — Linux release build helper
# Usage: ./scripts/build-linux.sh [--flatpak] [--deb] [--rpm] [--arch-pkg] [--check] [--fmt] [--clippy]
set -euo pipefail

FLATPAK=false
DEB=false
RPM=false
ARCH_PKG=false
CHECK=false
FMT=false
CLIPPY=false
COVERAGE=false

for arg in "$@"; do
  case "$arg" in
    --flatpak)   FLATPAK=true ;;
    --deb)       DEB=true ;;
    --rpm)       RPM=true ;;
    --arch-pkg)  ARCH_PKG=true ;;
    --check)     CHECK=true ;;
    --fmt)       FMT=true ;;
    --clippy)    CLIPPY=true ;;
    --coverage)  COVERAGE=true ;;
  esac
done

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[tributary]${NC} $*"; }
warn()  { echo -e "${YELLOW}[tributary]${NC} $*"; }
error() { echo -e "${RED}[tributary]${NC} $*" >&2; exit 1; }

# ── Dependency Checks ────────────────────────────────────────────────────────
info "Checking build dependencies..."

command -v cargo    &>/dev/null || error "cargo not found. Install Rust from https://rustup.rs"
command -v pkg-config &>/dev/null || error "pkg-config not found. Install: sudo apt install pkg-config  OR  sudo dnf install pkgconfig"

check_pkg() {
  pkg-config --exists "$1" 2>/dev/null || error "Missing pkg-config package: $1
  Debian/Ubuntu: sudo apt install $2
  Fedora:        sudo dnf install $3
  Arch:          sudo pacman -S $4"
}

check_pkg "gtk4"           "libgtk-4-dev"          "gtk4-devel"          "gtk4"
check_pkg "libadwaita-1"  "libadwaita-1-dev"      "libadwaita-devel"    "libadwaita"
check_pkg "gstreamer-1.0" "libgstreamer1.0-dev"   "gstreamer1-devel"    "gstreamer"
check_pkg "dbus-1"        "libdbus-1-dev"         "dbus-devel"          "dbus"

# Note: gst-plugins-good, gst-plugins-bad, gst-plugins-ugly, and gst-libav
# are runtime dependencies (not detectable via pkg-config).
# They are declared in the .deb, .rpm, and PKGBUILD package metadata.

info "All system dependencies satisfied."

# ── Quick-exit modes: --check, --fmt, --clippy ───────────────────────────────
if $FMT; then
  info "Running cargo fmt..."
  cargo fmt
  info "Formatting complete."
  exit 0
fi

if $CHECK; then
  info "Running cargo check..."
  cargo check
  info "Check passed."
  exit 0
fi

if $CLIPPY; then
  info "Running cargo clippy (pedantic)..."
  cargo clippy --all-targets -- -D warnings
  info "Clippy passed."
  exit 0
fi

if $COVERAGE; then
  command -v cargo-llvm-cov &>/dev/null || {
    info "Installing cargo-llvm-cov..."
    cargo install cargo-llvm-cov --locked
  }
  info "Running code coverage..."
  cargo llvm-cov --summary-only \
    --ignore-filename-regex '(ui/|jellyfin/|plex/|subsonic/|radio/|db/migration|desktop_integration/|main\.rs)'
  exit 0
fi

# ── Rust Build ───────────────────────────────────────────────────────────────
info "Building Tributary (release)..."
cargo build --release
info "Binary: $(pwd)/target/release/tributary"

# ── Install Icons (if running with --install or as root) ─────────────────────
ICON_PREFIX="${DESTDIR:-/usr/local}/share/icons/hicolor"
if [[ -d "data/icons/hicolor" ]]; then
  info "To install icons system-wide, run:"
  info "  sudo cp -r data/icons/hicolor/* /usr/local/share/icons/hicolor/"
fi

# ── Flatpak Bundle (optional) ────────────────────────────────────────────────
if $FLATPAK; then
  command -v flatpak-builder &>/dev/null || error "flatpak-builder not found. Install: sudo apt install flatpak-builder"
  command -v python3          &>/dev/null || error "python3 not found (required to generate cargo sources)"

  MANIFEST="build-aux/flatpak/io.github.tributary.Tributary.yml"
  info "Generating cargo-sources.json..."
  python3 build-aux/flatpak/flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json

  info "Building Flatpak bundle..."
  flatpak-builder --force-clean --repo=repo build-dir "$MANIFEST"
  flatpak build-bundle repo tributary.flatpak io.github.tributary.Tributary
  info "Flatpak bundle: $(pwd)/tributary.flatpak"
fi

# ── Debian Package (optional) ────────────────────────────────────────────────
if $DEB; then
  command -v cargo-deb &>/dev/null || {
    info "Installing cargo-deb..."
    cargo install cargo-deb
  }

  info "Building .deb package..."
  cargo deb
  DEB_FILE=$(ls target/debian/*.deb 2>/dev/null | head -1)
  if [[ -n "$DEB_FILE" ]]; then
    info "Debian package: $(pwd)/$DEB_FILE"
  else
    error "cargo-deb did not produce a .deb file"
  fi
fi

# ── RPM Package (optional) ───────────────────────────────────────────────────
if $RPM; then
  command -v cargo-generate-rpm &>/dev/null || {
    info "Installing cargo-generate-rpm..."
    cargo install cargo-generate-rpm
  }

  info "Building .rpm package..."
  cargo generate-rpm
  RPM_FILE=$(ls target/generate-rpm/*.rpm 2>/dev/null | head -1)
  if [[ -n "$RPM_FILE" ]]; then
    info "RPM package: $(pwd)/$RPM_FILE"
  else
    error "cargo-generate-rpm did not produce an .rpm file"
  fi
fi

# ── Arch Linux Package (optional) ────────────────────────────────────────────
if $ARCH_PKG; then
  command -v makepkg &>/dev/null || error "makepkg not found. This option requires Arch Linux (or an Arch-based distro)."

  info "Building Arch Linux package..."
  # Copy PKGBUILD to project root (makepkg expects it in cwd)
  cp build-aux/arch/PKGBUILD .

  # Extract version from Cargo.toml and patch PKGBUILD
  CARGO_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
  sed -i "s/^pkgver=.*/pkgver=${CARGO_VERSION}/" PKGBUILD

  makepkg -sf --noconfirm --skipchecksums
  PKG_FILE=$(ls *.pkg.tar.zst 2>/dev/null | head -1)
  if [[ -n "$PKG_FILE" ]]; then
    mkdir -p dist
    mv "$PKG_FILE" dist/
    info "Arch package: $(pwd)/dist/$PKG_FILE"
  else
    error "makepkg did not produce a .pkg.tar.zst file"
  fi

  # Clean up PKGBUILD from project root
  rm -f PKGBUILD
fi

info "Done."
