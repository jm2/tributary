#!/usr/bin/env bash
# scripts/build-linux.sh
# Tributary — Linux release build helper
# Usage: ./scripts/build-linux.sh [--flatpak]
set -euo pipefail

FLATPAK=false
for arg in "$@"; do
  [[ "$arg" == "--flatpak" ]] && FLATPAK=true
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

check_pkg "gtk4"        "libgtk-4-dev"     "gtk4-devel"       "gtk4"
check_pkg "libadwaita-1" "libadwaita-1-dev" "libadwaita-devel" "libadwaita"

info "All system dependencies satisfied."

# ── Rust Build ───────────────────────────────────────────────────────────────
info "Building Tributary (release)..."
cargo build --release
info "Binary: $(pwd)/target/release/tributary"

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

info "Done."
