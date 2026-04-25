#!/usr/bin/env bash
# scripts/build-macos.sh
# Tributary — macOS release build helper (.app + .dmg)
# Usage: ./scripts/build-macos.sh [--dmg] [--check] [--fmt] [--clippy]
set -euo pipefail

MAKE_DMG=false
CHECK=false
FMT=false
CLIPPY=false
COVERAGE=false
for arg in "$@"; do
  case "$arg" in
    --dmg)      MAKE_DMG=true ;;
    --check)    CHECK=true ;;
    --fmt)      FMT=true ;;
    --clippy)   CLIPPY=true ;;
    --coverage) COVERAGE=true ;;
  esac
done

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[tributary]${NC} $*"; }
warn()  { echo -e "${YELLOW}[tributary]${NC} $*"; }
error() { echo -e "${RED}[tributary]${NC} $*" >&2; exit 1; }

APP_NAME="Tributary"
BUNDLE_ID="io.github.tributary.Tributary"
BINARY="target/release/tributary"
APP_BUNDLE="dist/${APP_NAME}.app"

# Extract version from Cargo.toml
CARGO_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
info "Version: ${CARGO_VERSION}"

# ── Dependency Checks ────────────────────────────────────────────────────────
info "Checking build dependencies..."

command -v cargo &>/dev/null || error "cargo not found. Install Rust: https://rustup.rs"
command -v brew  &>/dev/null || error "Homebrew not found. Install: https://brew.sh"

for formula in gtk4 libadwaita pkg-config gstreamer gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav adwaita-icon-theme; do
  brew list "$formula" &>/dev/null || {
    warn "$formula not installed. Installing via Homebrew..."
    brew install "$formula"
  }
done

if $MAKE_DMG; then
  brew list create-dmg &>/dev/null || { warn "create-dmg not found. Installing..."; brew install create-dmg; }
fi

info "All system dependencies satisfied."

BREW_PREFIX="$(brew --prefix)"
export PKG_CONFIG_PATH="${BREW_PREFIX}/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

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
info "Binary built: $(pwd)/$BINARY"

# ── .app Bundle ──────────────────────────────────────────────────────────────
info "Creating .app bundle..."

rm -rf "$APP_BUNDLE"
mkdir -p "${APP_BUNDLE}/Contents/MacOS"
mkdir -p "${APP_BUNDLE}/Contents/Resources"
mkdir -p "${APP_BUNDLE}/Contents/Frameworks"

# Copy binary as Tributary-bin and create the bash wrapper
BIN_DEST="${APP_BUNDLE}/Contents/MacOS/${APP_NAME}"
cp "$BINARY" "${BIN_DEST}-bin"

# Force DYLD_LIBRARY_PATH
cat > "${BIN_DEST}" << 'EOF'
#!/bin/bash
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
BUNDLE_ROOT="$(dirname "$DIR")"

# Blind the app to Homebrew by stripping it from PATH
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"

# Force macOS to prioritize our bundled dylibs above everything else
export DYLD_LIBRARY_PATH="$BUNDLE_ROOT/Frameworks"

# Force GTK to look inside the bundle for icons and schemas
export XDG_DATA_DIRS="$BUNDLE_ROOT/Resources/share"
export GTK_DATA_PREFIX="$BUNDLE_ROOT/Resources"
export GDK_PIXBUF_MODULE_FILE="$BUNDLE_ROOT/Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"

# Force GStreamer to only use bundled plugins and scanners
export GST_PLUGIN_SYSTEM_PATH=""
export GST_PLUGIN_PATH="$BUNDLE_ROOT/Resources/lib/gstreamer-1.0"
export GST_PLUGIN_SCANNER="$DIR/gst-plugin-scanner"
# Set a bundle-local registry path so the first launch scans bundled plugins
export GST_REGISTRY="$DIR/gst-registry.bin"

# Launch the actual Rust binary
exec "$DIR/Tributary-bin" "$@"
EOF

chmod +x "${BIN_DEST}"

# ── Bundle GTK/Adwaita resources ─────────────────────────────────────────────
RESOURCES_DIR="${APP_BUNDLE}/Contents/Resources"

# Icons
mkdir -p "${RESOURCES_DIR}/share/icons"
cp -RL "${BREW_PREFIX}/share/icons/hicolor" "${RESOURCES_DIR}/share/icons/" 2>/dev/null || true
cp -RL "${BREW_PREFIX}/share/icons/Adwaita" "${RESOURCES_DIR}/share/icons/" 2>/dev/null || true

# Bundle the app's own hicolor icons (About dialog, etc.)
APP_ICONS_SRC="data/icons/hicolor"
if [[ -d "$APP_ICONS_SRC" ]]; then
  info "Bundling app hicolor icons..."
  cp -R "$APP_ICONS_SRC"/* "${RESOURCES_DIR}/share/icons/hicolor/" 2>/dev/null || true
fi

# Compile GTK Icon Caches (Fixes the missing SVG errors)
info "Compiling icon caches..."
command -v gtk4-update-icon-cache &>/dev/null && {
  gtk4-update-icon-cache -f -t "${RESOURCES_DIR}/share/icons/hicolor" 2>/dev/null || true
  gtk4-update-icon-cache -f -t "${RESOURCES_DIR}/share/icons/Adwaita" 2>/dev/null || true
}

# GLib schemas
mkdir -p "${RESOURCES_DIR}/share/glib-2.0/schemas"
cp -RL "${BREW_PREFIX}/share/glib-2.0/schemas" "${RESOURCES_DIR}/share/glib-2.0/" 2>/dev/null || true
glib-compile-schemas "${RESOURCES_DIR}/share/glib-2.0/schemas" 2>/dev/null || true

# GDK pixbuf loaders
PIXBUF_LOADER_DIR="${BREW_PREFIX}/lib/gdk-pixbuf-2.0"
if [[ -d "$PIXBUF_LOADER_DIR" ]]; then
  mkdir -p "${RESOURCES_DIR}/lib"
  cp -RL "$PIXBUF_LOADER_DIR" "${RESOURCES_DIR}/lib/" 2>/dev/null || true
fi

# ── Bundle GStreamer plugins ─────────────────────────────────────────────────
GST_PLUGIN_SRC="${BREW_PREFIX}/lib/gstreamer-1.0"
GST_PLUGIN_DEST="${RESOURCES_DIR}/lib/gstreamer-1.0"
if [[ -d "$GST_PLUGIN_SRC" ]]; then
  info "Bundling GStreamer plugins..."
  mkdir -p "$GST_PLUGIN_DEST"
  cp "${GST_PLUGIN_SRC}"/*.dylib "$GST_PLUGIN_DEST/" 2>/dev/null || true
  GST_PLUGIN_COUNT=$(ls -1 "$GST_PLUGIN_DEST"/*.dylib 2>/dev/null | wc -l | tr -d ' ')
  info "Bundled ${GST_PLUGIN_COUNT} GStreamer plugins."
else
  warn "GStreamer plugin directory not found at ${GST_PLUGIN_SRC}"
fi

# ── Bundle gst-plugin-scanner ────────────────────────────────────────────────
GST_SCANNER_DEST="${APP_BUNDLE}/Contents/MacOS/gst-plugin-scanner"
GST_SCANNER_SRC=""
for candidate in \
  "${BREW_PREFIX}/libexec/gstreamer-1.0/gst-plugin-scanner" \
  "${BREW_PREFIX}/Cellar/gstreamer"/*/libexec/gstreamer-1.0/gst-plugin-scanner \
  "${BREW_PREFIX}/opt/gstreamer/libexec/gstreamer-1.0/gst-plugin-scanner" \
  ; do
  for resolved in $candidate; do
    if [[ -f "$resolved" ]]; then
      GST_SCANNER_SRC="$resolved"
      break 2
    fi
  done
done

if [[ -n "$GST_SCANNER_SRC" ]]; then
  info "Bundling gst-plugin-scanner from ${GST_SCANNER_SRC}..."
  cp "$GST_SCANNER_SRC" "$GST_SCANNER_DEST"
  chmod u+w "$GST_SCANNER_DEST"
else
  warn "gst-plugin-scanner not found in any known Homebrew location!"
  warn "GStreamer playback may fail when launched from the .app bundle."
  # List what we searched for debugging
  warn "Searched: ${BREW_PREFIX}/libexec/gstreamer-1.0/"
  warn "Searched: ${BREW_PREFIX}/Cellar/gstreamer/*/libexec/gstreamer-1.0/"
  warn "Searched: ${BREW_PREFIX}/opt/gstreamer/libexec/gstreamer-1.0/"
fi

# Generate .icns from the iconset PNGs
ICONSET_SRC="data/tributary.iconset"
if [[ -d "$ICONSET_SRC" ]] && command -v iconutil &>/dev/null; then
  iconutil -c icns -o "${RESOURCES_DIR}/tributary.icns" "$ICONSET_SRC"
  info "App icon created via iconutil."
elif [[ -f "data/tributary.icns" ]]; then
  cp "data/tributary.icns" "${RESOURCES_DIR}/tributary.icns"
else
  warn "No app icon found — .app will use default icon."
fi

# Write Info.plist
# NOTE: LSEnvironment is intentionally omitted.  Its relative paths
# only work when the working directory is Contents/MacOS, which macOS
# does NOT guarantee when launching from Finder / Launchpad.  The
# binary's setup_macos_bundle_env() sets absolute paths at runtime.
cat > "${APP_BUNDLE}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>    <string>${APP_NAME}</string>
  <key>CFBundleIdentifier</key>   <string>${BUNDLE_ID}</string>
  <key>CFBundleName</key>         <string>${APP_NAME}</string>
  <key>CFBundleVersion</key>      <string>${CARGO_VERSION}</string>
  <key>CFBundlePackageType</key>  <string>APPL</string>
  <key>CFBundleIconFile</key>     <string>tributary</string>
  <key>NSHighResolutionCapable</key> <true/>
  <key>LSMinimumSystemVersion</key>  <string>13.0</string>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key>
      <string>Audio File</string>
      <key>CFBundleTypeRole</key>
      <string>Viewer</string>
      <key>LSHandlerRank</key>
      <string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.mp3</string>
        <string>public.mpeg-4-audio</string>
        <string>org.xiph.flac</string>
        <string>org.xiph.ogg-vorbis</string>
        <string>org.xiph.opus</string>
        <string>com.microsoft.waveform-audio</string>
        <string>public.aiff-audio</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# ── Copy and fix dylibs (recursive) ─────────────────────────────────────────
info "Bundling dylibs and fixing rpaths (recursive — this may take a moment)..."
FRAMEWORKS_DIR="${APP_BUNDLE}/Contents/Frameworks"

# We must point `otool` at the actual Rust binary (-bin), 
# not the bash wrapper we just created.
BIN="${APP_BUNDLE}/Contents/MacOS/${APP_NAME}-bin"

# Copy a single dylib into Frameworks/ if not already there.
copy_dylib() {
  local src="$1"
  local basename
  basename="$(basename "$src")"
  local dest="${FRAMEWORKS_DIR}/${basename}"
  [[ -f "$dest" ]] && return 1
  cp "$src" "$dest"
  chmod u+w "$dest"
  install_name_tool -id "@executable_path/../Frameworks/${basename}" "$dest" 2>/dev/null || true
  return 0
}

# The Aggressive @rpath Fix
fix_rpaths() {
  local bin="$1"
  local new_libs=()
  while IFS= read -r libpath; do
    local basename
    basename="$(basename "$libpath")"
    
    # Resolve actual Homebrew path if it's a relative @rpath or @loader_path
    local src_path="$libpath"
    if [[ "$libpath" == @rpath/* ]] || [[ "$libpath" == @loader_path/* ]]; then
      src_path="${BREW_PREFIX}/lib/${basename}"
    fi
    
    if [[ -f "$src_path" ]]; then
      if copy_dylib "$src_path"; then
        new_libs+=("${FRAMEWORKS_DIR}/${basename}")
      fi
    fi
    
    install_name_tool -change "$libpath" \
      "@executable_path/../Frameworks/${basename}" "$bin" 2>/dev/null || true
      
  # The updated regex catches /opt/homebrew, /usr/local, AND @rpath/@loader_path
  done < <(otool -L "$bin" 2>/dev/null \
    | awk '/\/opt\/homebrew|\/usr\/local|@rpath\/|@loader_path\// {print $1}')
    
  NEWLY_COPIED=("${new_libs[@]+"${new_libs[@]}"}")
}

# Fix the main binary
fix_rpaths "$BIN"
QUEUE=("${NEWLY_COPIED[@]+"${NEWLY_COPIED[@]}"}")

PASS=1
while [[ ${#QUEUE[@]} -gt 0 ]]; do
  info "  Dylib pass ${PASS}: processing ${#QUEUE[@]} libraries..."
  NEXT_QUEUE=()
  for lib in "${QUEUE[@]}"; do
    fix_rpaths "$lib"
    NEXT_QUEUE+=("${NEWLY_COPIED[@]+"${NEWLY_COPIED[@]}"}")
  done
  QUEUE=("${NEXT_QUEUE[@]+"${NEXT_QUEUE[@]}"}")
  PASS=$((PASS + 1))
  # Safety valve: prevent infinite loops
  if [[ $PASS -gt 20 ]]; then
    warn "Dylib recursion exceeded 20 passes — stopping."
    break
  fi
done

TOTAL_DYLIBS=$(ls -1 "${FRAMEWORKS_DIR}"/*.dylib 2>/dev/null | wc -l | tr -d ' ')
info "Bundled ${TOTAL_DYLIBS} dylibs into Frameworks/."

# Fix GStreamer plugins
if [[ -d "$GST_PLUGIN_DEST" ]]; then
  info "Fixing rpaths in GStreamer plugins..."
  for plugin in "${GST_PLUGIN_DEST}"/*.dylib; do
    [[ -f "$plugin" ]] || continue
    chmod u+w "$plugin"
    install_name_tool -add_rpath "@loader_path/../../../Frameworks" "$plugin" 2>/dev/null || true
    fix_rpaths "$plugin"
  done
fi

# Fix rpaths in the bundled gst-plugin-scanner
if [[ -f "$GST_SCANNER_DEST" ]]; then
  info "Fixing rpaths in gst-plugin-scanner..."
  fix_rpaths "$GST_SCANNER_DEST"
fi

# (Stale GStreamer registry cleanup moved to end of script,
# after code signing, to prevent re-generation.)

# Fix rpaths in pixbuf loaders
PIXBUF_LOADERS_DEST="${RESOURCES_DIR}/lib/gdk-pixbuf-2.0/2.10.0/loaders"
if [[ -d "$PIXBUF_LOADERS_DEST" ]]; then
  info "Fixing rpaths in pixbuf loaders..."
  for loader in "${PIXBUF_LOADERS_DEST}"/*.so "${PIXBUF_LOADERS_DEST}"/*.dylib; do
    [[ -f "$loader" ]] || continue
    chmod u+w "$loader"
    fix_rpaths "$loader"
  done
fi

# Update pixbuf loaders.cache
PIXBUF_CACHE="${RESOURCES_DIR}/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
if [[ -f "$PIXBUF_CACHE" ]]; then
  info "Patching pixbuf loaders.cache for bundle paths..."
  sed -i '' "s|${BREW_PREFIX}/lib/gdk-pixbuf-2.0/2.10.0/loaders/|${RESOURCES_DIR}/lib/gdk-pixbuf-2.0/2.10.0/loaders/|g" "$PIXBUF_CACHE" 2>/dev/null || true
fi

# Verify critical GStreamer plugins for audio playback
if [[ -d "$GST_PLUGIN_DEST" ]]; then
  for critical_plugin in libgstcoreelements libgstisomp4 libgstlibav libgstaudioparsers libgstaudioconvert libgstaudioresample libgstosxaudio; do
    if ! ls "$GST_PLUGIN_DEST"/${critical_plugin}.dylib 1>/dev/null 2>&1; then
      warn "Missing critical GStreamer plugin: ${critical_plugin}"
    fi
  done
fi

# Verify Adwaita icons
ADWAITA_SCALABLE="${RESOURCES_DIR}/share/icons/Adwaita/scalable"
if [[ -d "$ADWAITA_SCALABLE" ]]; then
  ADWAITA_SVG_COUNT=$(find "$ADWAITA_SCALABLE" -name '*.svg' 2>/dev/null | wc -l | tr -d ' ')
  info "Adwaita scalable icons: ${ADWAITA_SVG_COUNT} SVGs found."
fi

# ── Ad-hoc Code Signing ─────────────────────────────────────────────────────
# macOS 13+ kills unsigned binaries launched from .app bundles (SIGKILL / exit 9).
# After install_name_tool modifies binaries, any existing signature is invalidated.
# We must re-sign everything with at least an ad-hoc signature.
info "Ad-hoc code signing the bundle..."

find "${FRAMEWORKS_DIR}" -name '*.dylib' -exec codesign --force --sign - {} \; 2>/dev/null || true
[[ -d "$GST_PLUGIN_DEST" ]] && find "$GST_PLUGIN_DEST" -name '*.dylib' -exec codesign --force --sign - {} \; 2>/dev/null || true
[[ -d "$PIXBUF_LOADERS_DEST" ]] && find "$PIXBUF_LOADERS_DEST" \( -name '*.so' -o -name '*.dylib' \) -exec codesign --force --sign - {} \; 2>/dev/null || true
[[ -f "$GST_SCANNER_DEST" ]] && codesign --force --sign - "$GST_SCANNER_DEST" 2>/dev/null || true

codesign --force --sign - "$BIN" 2>/dev/null || true
codesign --force --deep --sign - "$APP_BUNDLE" 2>/dev/null || true

info ".app bundle created: $(pwd)/${APP_BUNDLE}"

if codesign --verify --verbose "$APP_BUNDLE" 2>/dev/null; then
  info "Code signature verified OK."
else
  warn "Code signature verification failed."
fi

# Remove any stale GStreamer registry that the build may have generated.
# This must happen AFTER code signing — if the registry is present during
# signing, its hash goes into the seal and first-launch will invalidate it
# when GStreamer overwrites it.  Deleting it here forces a clean first-launch
# scan that writes a registry referencing the bundle-local plugin paths.
rm -f "${APP_BUNDLE}/Contents/MacOS/gst-registry.bin"

# ── DMG ──────────────────────────────────────────────────────────────────────
if $MAKE_DMG; then
  info "Creating .dmg disk image..."
  mkdir -p dist
  rm -f "dist/${APP_NAME}.dmg"
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
