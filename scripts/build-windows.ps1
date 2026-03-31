<#
.SYNOPSIS
    Tributary — Windows release build helper

.DESCRIPTION
    Requires: MSYS2, Rust, cargo in PATH. Compiles the Rust application and 
    bundles it with all required GTK4/MSYS2 DLLs and assets into a zip file.

.PARAMETER Msys2Root
    The root directory of the MSYS2 installation. Defaults to "C:\msys64".

.PARAMETER SkipBundle
    If specified, just compiles the binary without DLL bundling.

.PARAMETER NoCargoBuild
    If specified, skips the cargo build step (useful for CI).
#>
param(
    [string]$Msys2Root = "C:\msys64",
    [switch]$SkipBundle,
    [switch]$NoCargoBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Write-Info  { Write-Host "[tributary] $args" -ForegroundColor Green  }
function Write-Warn  { Write-Host "[tributary] $args" -ForegroundColor Yellow }
function Write-Err   { Write-Host "[tributary] $args" -ForegroundColor Red; exit 1 }

$RustTarget = if ($env:RUST_TARGET) { $env:RUST_TARGET } else { "x86_64-pc-windows-gnu" }
$MsysEnv    = if ($env:MSYS_ENV) { $env:MSYS_ENV } else { "ucrt64" }

# Map the environment to the correct MSYS2 package prefix for error messages
$PkgPrefix = switch ($MsysEnv) {
    "ucrt64"     { "mingw-w64-ucrt-x86_64" }
    "clangarm64" { "mingw-w64-clang-aarch64" }
    default      { "mingw-w64-$MsysEnv" }
}

$MsysPath = Join-Path $Msys2Root $MsysEnv
$DIST     = "dist\tributary-windows"

# ── Dependency Checks ────────────────────────────────────────────────────────
Write-Info "Checking build dependencies..."

if (-not (Test-Path $Msys2Root)) {
    Write-Err "MSYS2 not found at $Msys2Root. Install from https://www.msys2.org"
}

if (-not $NoCargoBuild -and -not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Err "cargo not found. Install Rust from https://rustup.rs"
}

# ── PKG_CONFIG setup ─────────────────────────────────────────────────────────
$pkgConfigPath = Join-Path $MsysPath "lib\pkgconfig"
if (-not (Test-Path $pkgConfigPath)) {
    Write-Err "pkgconfig directory not found at $pkgConfigPath.`nIn MSYS2 shell, run:`n  pacman -S $PkgPrefix-pkg-config $PkgPrefix-toolchain"
}

$env:PKG_CONFIG_PATH        = $pkgConfigPath
$env:PKG_CONFIG_ALLOW_CROSS = "1"
$env:PATH = "$MsysPath\bin;" + $env:PATH

# Force Cargo to use MSYS2 tools instead of Rustup's incomplete bundled toolchain.
$env:DLLTOOL = Join-Path $MsysPath "bin\dlltool.exe"
$env:CC      = Join-Path $MsysPath "bin\gcc.exe"
$env:CXX     = Join-Path $MsysPath "bin\g++.exe"
$env:AR      = Join-Path $MsysPath "bin\ar.exe"

Write-Info "PKG_CONFIG_PATH set to $pkgConfigPath"

# ── Per-package dependency checks (mirrors build-linux.sh) ───────────────────
$pkgConfig = Join-Path $MsysPath "bin\pkg-config.exe"

# Compile-time libraries (hard fail)
$requiredPkgs = @(
    @{ pc = "gtk4";           pkg = "gtk4" },
    @{ pc = "libadwaita-1";   pkg = "libadwaita" },
    @{ pc = "gstreamer-1.0";  pkg = "gstreamer" }
)

$missing = @()
foreach ($dep in $requiredPkgs) {
    $rc = & $pkgConfig --exists $dep.pc 2>$null; $ok = $LASTEXITCODE -eq 0
    if ($ok) {
        Write-Host "  [ok] $($dep.pc)"
    } else {
        Write-Host "  [MISSING] $($dep.pc)" -ForegroundColor Red
        $missing += "$PkgPrefix-$($dep.pkg)"
    }
}
if ($missing.Count -gt 0) {
    Write-Err "Missing compile-time packages. In MSYS2 shell, run:`n  pacman -S $($missing -join ' ')"
}

# Runtime GStreamer plugins (warn only — not needed to compile)
$gstPluginDir = Join-Path $MsysPath "lib\gstreamer-1.0"
$pluginWarnings = @()
foreach ($plugin in @("gst-plugins-good", "gst-plugins-bad", "gst-libav")) {
    # Each package installs DLLs with a recognisable prefix into the plugin dir
    $pattern = switch ($plugin) {
        "gst-plugins-good" { "libgstaudioparsers.dll" }
        "gst-plugins-bad"  { "libgstfdkaac.dll" }
        "gst-libav"        { "libgstlibav.dll" }
    }
    $probe = Join-Path $gstPluginDir $pattern
    if (Test-Path $probe) {
        Write-Host "  [ok] $plugin"
    } else {
        Write-Host "  [MISSING] $plugin (audio codecs)" -ForegroundColor Yellow
        $pluginWarnings += "$PkgPrefix-$plugin"
    }
}
if ($pluginWarnings.Count -gt 0) {
    Write-Warn "Missing GStreamer codec plugins — playback of some formats will fail.`n  pacman -S $($pluginWarnings -join ' ')"
}

Write-Info "All dependency checks passed."

# ── Rust Build ───────────────────────────────────────────────────────────────
if (-not $NoCargoBuild) {
    Write-Info "Building Tributary (release) for $RustTarget..."
    cargo build --release --target $RustTarget
} else {
    Write-Info "Skipping cargo build (-NoCargoBuild specified)."
}

$exePath = "target\$RustTarget\release\tributary.exe"
if (-not (Test-Path $exePath)) {
    Write-Err "Binary not found at $exePath"
}

Write-Info "Binary: $((Get-Item $exePath).FullName)"

if ($SkipBundle) {
    Write-Info "Skipping DLL bundle (--SkipBundle specified). Done."
    exit 0
}

# ── DLL Bundle ───────────────────────────────────────────────────────────────
Write-Info "Bundling GTK4 DLLs and resources into $DIST ..."

Remove-Item -Recurse -Force $DIST -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $DIST | Out-Null

# Copy the binary
Copy-Item $exePath $DIST

# Resolve DLL dependencies with ldd (available in MSYS2)
$ldd = Join-Path $MsysPath "bin\ldd.exe"
if (-not (Test-Path $ldd)) { $ldd = "ldd" }

Write-Info "Resolving DLLs with ldd..."

& $ldd $exePath 2>$null |
    Select-String "/$MsysEnv/bin/" |
    ForEach-Object {
        $parts = $_.Line -split "\s+"
        # ldd output: libname => /path/to/lib (0xaddr)
        $libPath = $parts | Where-Object { $_ -like "*$MsysEnv/bin*" } | Select-Object -First 1
        if ($libPath -and (Test-Path $libPath)) {
            $dest = Join-Path $DIST (Split-Path $libPath -Leaf)
            if (-not (Test-Path $dest)) {
                Copy-Item $libPath $dest
                Write-Host "  copied: $(Split-Path $libPath -Leaf)"
            }
        }
    }

# ── GStreamer Plugins (runtime-loaded, invisible to ldd) ─────────────────────
Write-Info "Copying GStreamer plugins..."

$gstPluginSrc  = Join-Path $MsysPath "lib\gstreamer-1.0"
$gstPluginDest = Join-Path $DIST     "lib\gstreamer-1.0"
if (Test-Path $gstPluginSrc) {
    New-Item -ItemType Directory -Force $gstPluginDest | Out-Null
    Copy-Item "$gstPluginSrc\*.dll" $gstPluginDest
    $pluginCount = (Get-ChildItem "$gstPluginDest\*.dll").Count
    Write-Info "GStreamer plugins copied ($pluginCount plugins)."

    # Resolve transitive DLL dependencies from plugin DLLs
    Write-Info "Resolving additional DLLs from GStreamer plugins..."
    Get-ChildItem "$gstPluginDest\*.dll" | ForEach-Object {
        & $ldd $_.FullName 2>$null |
            Select-String "/$MsysEnv/bin/" |
            ForEach-Object {
                $parts = $_.Line -split "\s+"
                $libPath = $parts | Where-Object { $_ -like "*$MsysEnv/bin*" } | Select-Object -First 1
                if ($libPath -and (Test-Path $libPath)) {
                    $dest = Join-Path $DIST (Split-Path $libPath -Leaf)
                    if (-not (Test-Path $dest)) {
                        Copy-Item $libPath $dest
                        Write-Host "  copied: $(Split-Path $libPath -Leaf)"
                    }
                }
            }
    }
} else {
    Write-Warn "GStreamer plugins not found at $gstPluginSrc — audio playback will not work."
    Write-Warn "Install in MSYS2: pacman -S $PkgPrefix-gst-plugins-good $PkgPrefix-gst-plugins-bad $PkgPrefix-gst-libav"
}

# ── GTK Resources ────────────────────────────────────────────────────────────
Write-Info "Copying GTK icons and schemas..."

# Icon themes (required for symbolic icons used in the UI)
foreach ($theme in @("hicolor", "Adwaita")) {
    $src  = Join-Path $MsysPath "share\icons\$theme"
    $dest = Join-Path $DIST   "share\icons\$theme"
    if (Test-Path $src) {
        Copy-Item -Recurse -Force $src (Split-Path $dest) | Out-Null
    }
}

# GLib schemas
$schemasSrc  = Join-Path $MsysPath "share\glib-2.0\schemas"
$schemasDest = Join-Path $DIST   "share\glib-2.0\schemas"
if (Test-Path $schemasSrc) {
    New-Item -ItemType Directory -Force $schemasDest | Out-Null
    Copy-Item "$schemasSrc\*.xml" $schemasDest -ErrorAction SilentlyContinue
    # Compile schemas
    $compiler = Join-Path $MsysPath "bin\glib-compile-schemas.exe"
    if (Test-Path $compiler) {
        & $compiler $schemasDest
        Write-Info "Schemas compiled."
    }
}

# GdkPixbuf loaders (required for image rendering)
$loadersSrc  = Join-Path $MsysPath "lib\gdk-pixbuf-2.0"
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
