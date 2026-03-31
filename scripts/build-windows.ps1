# scripts/build-windows.ps1
# Tributary — Windows release build helper
# Requires: MSYS2 (ucrt64), Rust (stable-x86_64-pc-windows-gnu), cargo in PATH
#
# Usage:
#   .\scripts\build-windows.ps1
#   .\scripts\build-windows.ps1 -SkipBundle   # just compile, no DLL bundling
#   .\scripts\build-windows.ps1 -Msys2Root "D:\msys64"

param(
    [string]$Msys2Root = "C:\msys64",
    [switch]$SkipBundle
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Write-Info  { Write-Host "[tributary] $args" -ForegroundColor Green  }
function Write-Warn  { Write-Host "[tributary] $args" -ForegroundColor Yellow }
function Write-Err   { Write-Host "[tributary] $args" -ForegroundColor Red; exit 1 }

$UCRT64 = Join-Path $Msys2Root "ucrt64"
$DIST   = "dist\tributary-windows"

# ── Dependency Checks ────────────────────────────────────────────────────────
Write-Info "Checking build dependencies..."

if (-not (Test-Path $Msys2Root)) {
    Write-Err "MSYS2 not found at $Msys2Root. Install from https://www.msys2.org"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Err "cargo not found. Install Rust from https://rustup.rs"
}

# ── PKG_CONFIG setup ─────────────────────────────────────────────────────────
$pkgConfigPath = Join-Path $UCRT64 "lib\pkgconfig"
if (-not (Test-Path $pkgConfigPath)) {
    Write-Err "GTK4 pkgconfig not found at $pkgConfigPath.`nIn MSYS2 UCRT64 shell, run:`n  pacman -S mingw-w64-ucrt-x86_64-gtk4 mingw-w64-ucrt-x86_64-libadwaita"
}

$env:PKG_CONFIG_PATH   = $pkgConfigPath
$env:PKG_CONFIG_ALLOW_CROSS = "1"
$env:PATH = "$UCRT64\bin;" + $env:PATH

Write-Info "PKG_CONFIG_PATH set to $pkgConfigPath"
Write-Info "All dependency checks passed."

# ── Rust Build ───────────────────────────────────────────────────────────────
Write-Info "Building Tributary (release)..."
cargo build --release
Write-Info "Binary: $((Get-Item 'target\release\tributary.exe').FullName)"

if ($SkipBundle) {
    Write-Info "Skipping DLL bundle (--SkipBundle specified). Done."
    exit 0
}

# ── DLL Bundle ───────────────────────────────────────────────────────────────
Write-Info "Bundling GTK4 DLLs and resources into $DIST ..."

Remove-Item -Recurse -Force $DIST -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $DIST | Out-Null

# Copy the binary
Copy-Item "target\release\tributary.exe" $DIST

# Resolve DLL dependencies with ldd (available in MSYS2)
$ldd = Join-Path $UCRT64 "bin\ldd.exe"
if (-not (Test-Path $ldd)) { $ldd = "ldd" }

$exePath = "target\release\tributary.exe"
Write-Info "Resolving DLLs with ldd..."

& $ldd $exePath 2>$null |
    Select-String "/ucrt64/bin/" |
    ForEach-Object {
        $parts = $_.Line -split "\s+"
        # ldd output: libname => /path/to/lib (0xaddr)
        $libPath = $parts | Where-Object { $_ -like "*ucrt64/bin*" } | Select-Object -First 1
        if ($libPath -and (Test-Path $libPath)) {
            $dest = Join-Path $DIST (Split-Path $libPath -Leaf)
            if (-not (Test-Path $dest)) {
                Copy-Item $libPath $dest
                Write-Host "  copied: $(Split-Path $libPath -Leaf)"
            }
        }
    }

# ── GTK Resources ────────────────────────────────────────────────────────────
Write-Info "Copying GTK icons and schemas..."

# Icon themes (required for symbolic icons used in the UI)
foreach ($theme in @("hicolor", "Adwaita")) {
    $src  = Join-Path $UCRT64 "share\icons\$theme"
    $dest = Join-Path $DIST   "share\icons\$theme"
    if (Test-Path $src) {
        Copy-Item -Recurse -Force $src (Split-Path $dest) | Out-Null
    }
}

# GLib schemas
$schemasSrc  = Join-Path $UCRT64 "share\glib-2.0\schemas"
$schemasDest = Join-Path $DIST   "share\glib-2.0\schemas"
if (Test-Path $schemasSrc) {
    New-Item -ItemType Directory -Force $schemasDest | Out-Null
    Copy-Item "$schemasSrc\*.xml" $schemasDest -ErrorAction SilentlyContinue
    # Compile schemas
    $compiler = Join-Path $UCRT64 "bin\glib-compile-schemas.exe"
    if (Test-Path $compiler) {
        & $compiler $schemasDest
        Write-Info "Schemas compiled."
    }
}

# GdkPixbuf loaders (required for image rendering)
$loadersSrc  = Join-Path $UCRT64 "lib\gdk-pixbuf-2.0"
$loadersDest = Join-Path $DIST   "lib\gdk-pixbuf-2.0"
if (Test-Path $loadersSrc) {
    Copy-Item -Recurse -Force $loadersSrc (Split-Path $loadersDest) | Out-Null
    Write-Info "GdkPixbuf loaders copied."
}

# ── Zip Archive ──────────────────────────────────────────────────────────────
Write-Info "Creating zip archive..."
$zipPath = "dist\tributary-windows.zip"
Remove-Item $zipPath -ErrorAction SilentlyContinue
Compress-Archive -Path $DIST -DestinationPath $zipPath
Write-Info "Archive created: $((Get-Item $zipPath).FullName)"

Write-Info "Done."
