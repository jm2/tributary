#!/usr/bin/env bash
# scripts/build-macos.sh
# Tributary — macOS release build helper (.app + .dmg)
# Usage: ./scripts/build-macos.sh [--dmg]
set -euo pipefail

MAKE_DMG=false
for arg in "$@"; do
  [[ "$arg" == "--dmg" ]] && MAKE_DMG=true
done

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[tributary]${NC} $*"; }
warn()  { echo -e "${YELLOW}[tributary]${NC} $*"; }
error() { echo -e "${RED}[tributary]${NC} $*" >&2; exit 1; }

APP_NAME="Tributary"
BUNDLE_ID="io.github.tributary.Tributary"
BINARY="target/release/tributary"
APP_BUNDLE="dist/${APP_NAME}.app"

# ── Dependency Checks ────────────────────────────────────────────────────────
info "Checking build dependencies..."

command -v cargo &>/dev/null || error "cargo not found. Install Rust: https://rustup.rs"
command -v brew  &>/dev/null || error "Homebrew not found. Install: https://brew.sh"

for formula in gtk4 libadwaita pkg-config gstreamer; do
  brew list "$formula" &>/dev/null || {
    warn "$formula not installed. Installing via Homebrew..."
    brew install "$formula"
  }
done

if $MAKE_DMG; then
  brew list create-dmg &>/dev/null || { warn "create-dmg not found. Installing..."; brew install create-dmg; }
fi

info "All system dependencies satisfied."

# ── Rust Build ───────────────────────────────────────────────────────────────
info "Building Tributary (release)..."

# Homebrew on Apple Silicon uses /opt/homebrew; Intel uses /usr/local
BREW_PREFIX="$(brew --prefix)"
export PKG_CONFIG_PATH="${BREW_PREFIX}/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

cargo build --release
info "Binary built: $(pwd)/$BINARY"

# ── .app Bundle ──────────────────────────────────────────────────────────────
info "Creating .app bundle..."

rm -rf "$APP_BUNDLE"
mkdir -p "${APP_BUNDLE}/Contents/MacOS"
mkdir -p "${APP_BUNDLE}/Contents/Resources"
mkdir -p "${APP_BUNDLE}/Contents/Frameworks"

# Copy binary
cp "$BINARY" "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}"

# Write Info.plist
cat > "${APP_BUNDLE}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>    <string>${APP_NAME}</string>
  <key>CFBundleIdentifier</key>   <string>${BUNDLE_ID}</string>
  <key>CFBundleName</key>         <string>${APP_NAME}</string>
  <key>CFBundleVersion</key>      <string>0.1.0</string>
  <key>CFBundlePackageType</key>  <string>APPL</string>
  <key>NSHighResolutionCapable</key> <true/>
  <key>LSMinimumSystemVersion</key>  <string>13.0</string>
</dict>
</plist>
PLIST

# Copy GTK shared libraries and fix up rpaths
info "Bundling dylibs and fixing rpaths (this may take a moment)..."
FRAMEWORKS_DIR="${APP_BUNDLE}/Contents/Frameworks"
BIN="${APP_BUNDLE}/Contents/MacOS/${APP_NAME}"

# Collect unique dylibs reported by otool, excluding system frameworks
copy_and_fix_dylib() {
  local src="$1"
  local basename
  basename="$(basename "$src")"
  local dest="${FRAMEWORKS_DIR}/${basename}"
  [[ -f "$dest" ]] && return
  cp "$src" "$dest"
  # Fix the install name of the copied library
  install_name_tool -id "@executable_path/../Frameworks/${basename}" "$dest" 2>/dev/null || true
}

fix_binary_rpaths() {
  local bin="$1"
  # Replace absolute Homebrew paths with @executable_path-relative ones
  otool -L "$bin" 2>/dev/null \
    | awk '/\/opt\/homebrew|\/usr\/local/{print $1}' \
    | while read -r libpath; do
        local basename
        basename="$(basename "$libpath")"
        copy_and_fix_dylib "$libpath"
        install_name_tool -change "$libpath" \
          "@executable_path/../Frameworks/${basename}" "$bin" 2>/dev/null || true
      done
}

fix_binary_rpaths "$BIN"

info ".app bundle created: $(pwd)/${APP_BUNDLE}"

# ── DMG ──────────────────────────────────────────────────────────────────────
if $MAKE_DMG; then
  info "Creating .dmg disk image..."
  mkdir -p dist
  create-dmg \
    --volname "${APP_NAME}" \
    --window-pos 200 120 \
    --window-size 600 400 \
    --icon-size 100 \
    --app-drop-link 450 185 \
    "dist/${APP_NAME}.dmg" \
    "${APP_BUNDLE}"
  info "DMG created: $(pwd)/dist/${APP_NAME}.dmg"
fi

info "Done."
