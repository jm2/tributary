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

.PARAMETER InnoSetup
    If specified, builds an Inno Setup installer (.exe) from the bundled dist folder.
    Requires Inno Setup 6 to be installed (iscc.exe in PATH or standard install location).

.PARAMETER Check
    If specified, sets up the MSYS2 build environment and runs `cargo check` only.
    Useful for quick compilation checking from PowerShell without a full build.

.PARAMETER Clippy
    If specified, sets up the MSYS2 build environment and runs `cargo clippy -- -D warnings`.
    Useful for running the Clippy linter from PowerShell without a full build.

.PARAMETER Fmt
    If specified, sets up the MSYS2 build environment and runs `cargo fmt` only.
    Useful for formatting code from PowerShell without a full build.

.PARAMETER CargoUpdate
    If specified, sets up the MSYS2 build environment and runs `cargo update` with
    any additional arguments passed via -CargoUpdateArgs. Useful for updating
    dependencies from PowerShell (e.g. -CargoUpdate -CargoUpdateArgs "-p rustls-webpki").
#>
param(
    [string]$Msys2Root = "C:\msys64",
    [switch]$SkipBundle,
    [switch]$NoCargoBuild,
    [switch]$InnoSetup,
    [switch]$Check,
    [switch]$Clippy,
    [switch]$Fmt,
    [switch]$Coverage,
    [switch]$CargoUpdate,
    [string]$CargoUpdateArgs = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Write-Info { Write-Host "[tributary] $args" -ForegroundColor Green }
function Write-Warn { Write-Host "[tributary] $args" -ForegroundColor Yellow }
function Write-Err { Write-Host "[tributary] $args" -ForegroundColor Red; exit 1 }

# Auto-detect ARM64 when env vars are not explicitly set.
$NativeArch = if ([System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -eq [System.Runtime.InteropServices.Architecture]::Arm64) {
    "arm64"
}
elseif ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    "arm64"
}
else {
    "x64"
}

$RustTarget = if ($env:RUST_TARGET) { $env:RUST_TARGET } elseif ($NativeArch -eq "arm64") { "aarch64-pc-windows-gnullvm" } else { "x86_64-pc-windows-gnullvm" }
$MsysEnv = if ($env:MSYS_ENV) { $env:MSYS_ENV } elseif ($NativeArch -eq "arm64") { "clangarm64" } else { "clang64" }

# Map the environment to the correct MSYS2 package prefix for error messages
$PkgPrefix = switch ($MsysEnv) {
    "clang64" { "mingw-w64-clang-x86_64" }
    "clangarm64" { "mingw-w64-clang-aarch64" }
    "ucrt64" { "mingw-w64-ucrt-x86_64" }
    default { "mingw-w64-$MsysEnv" }
}

$MsysPath = Join-Path $Msys2Root $MsysEnv
$DIST = "dist\tributary-windows"

# ── Inno Setup only mode ─────────────────────────────────────────────────────
# When -InnoSetup is passed with -SkipBundle, skip straight to installer creation
if ($InnoSetup -and $SkipBundle) {
    Write-Info "Building Inno Setup installer..."

    # Determine architecture for Inno Setup
    $InnoArch = if ($env:INNO_ARCH) { $env:INNO_ARCH } else { "x64" }

    # Extract version from Cargo.toml
    $CargoVersion = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches.Groups[1].Value

    # Find iscc.exe
    $iscc = $null
    $isccPaths = @(
        "iscc.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe",
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $isccPaths) {
        if (Get-Command $p -ErrorAction SilentlyContinue) { $iscc = $p; break }
        if (Test-Path $p) { $iscc = $p; break }
    }
    if (-not $iscc) {
        Write-Err "Inno Setup compiler (iscc.exe) not found. Install Inno Setup 6 from https://jrsoftware.org/isinfo.php"
    }

    $issFile = "build-aux\inno\tributary.iss"
    $sourceDir = (Resolve-Path $DIST).Path
    $outputDir = (Resolve-Path "dist").Path

    Write-Info "Running Inno Setup compiler..."
    & $iscc /DAppVersion="$CargoVersion" /DSourceDir="$sourceDir" /DOutputDir="$outputDir" /DTargetArch="$InnoArch" $issFile
    if ($LASTEXITCODE -ne 0) { Write-Err "Inno Setup compilation failed." }

    Write-Info "Installer created: $outputDir\tributary-setup.exe"
    Write-Info "Done."
    exit 0
}

# ── Dependency Checks ────────────────────────────────────────────────────────
Write-Info "Checking build dependencies..."

if (-not (Test-Path $Msys2Root)) {
    Write-Err "MSYS2 not found at $Msys2Root. Install from https://www.msys2.org"
}

$cargoNeeded = (-not $NoCargoBuild) -or $Check -or $Clippy -or $Fmt -or $CargoUpdate
if ($cargoNeeded -and -not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Err "cargo not found. Install Rustup from https://rustup.rs or winget install Rustlang.Rustup"
}

if ($cargoNeeded) {
    $targetLibDir = rustc --target $RustTarget --print target-libdir 2>$null
    if (-not $targetLibDir -or -not (Test-Path $targetLibDir)) {
        if (Get-Command rustup -ErrorAction SilentlyContinue) {
            Write-Info "Ensuring Rust target $RustTarget is installed via rustup..."
            $null = rustup target add $RustTarget 2>&1
            if ($LASTEXITCODE -ne 0) {
                Write-Err "Failed to install target $RustTarget with rustup."
            }
        }
        else {
            Write-Err "Rust target '$RustTarget' is missing from your compiler, and rustup is not available. Please install rustup via https://rustup.rs or winget install Rustlang.Rustup"
        }
    }
}

# ── PKG_CONFIG setup ─────────────────────────────────────────────────────────
$pkgConfigPath = Join-Path $MsysPath "lib\pkgconfig"
$pkgConfigExe = Join-Path $MsysPath "bin\pkg-config.exe"

if (-not (Test-Path $pkgConfigExe)) {
    Write-Err "pkgconfig executable not found in $MsysPath\bin.`nIn MSYS2 shell, run:`n  pacman -S $PkgPrefix-pkg-config $PkgPrefix-toolchain"
}

$env:PKG_CONFIG_PATH = $pkgConfigPath
$env:PKG_CONFIG_ALLOW_CROSS = "1"
$env:PATH = "$MsysPath\bin;" + $env:PATH

# Force Cargo to use MSYS2 tools instead of Rustup's incomplete bundled toolchain.
if ($MsysEnv -match "clang") {
    $env:DLLTOOL = Join-Path $MsysPath "bin\llvm-dlltool.exe"
    $env:CC = Join-Path $MsysPath "bin\clang.exe"
    $env:CXX = Join-Path $MsysPath "bin\clang++.exe"
    $env:AR = Join-Path $MsysPath "bin\llvm-ar.exe"
}
else {
    $env:DLLTOOL = Join-Path $MsysPath "bin\dlltool.exe"
    $env:CC = Join-Path $MsysPath "bin\gcc.exe"
    $env:CXX = Join-Path $MsysPath "bin\g++.exe"
    $env:AR = Join-Path $MsysPath "bin\ar.exe"
}

Write-Info "PKG_CONFIG_PATH set to $pkgConfigPath"

# ── Quick-exit modes: --Check and --Fmt ──────────────────────────────────────
if ($Fmt) {
    Write-Info "Running cargo fmt..."
    cargo fmt
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo fmt failed." }
    Write-Info "Formatting complete."
    exit 0
}

if ($Check) {
    Write-Info "Running cargo check for $RustTarget..."
    cargo check --target $RustTarget
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo check failed." }
    Write-Info "Check passed."
    exit 0
}

if ($Clippy) {
    Write-Info "Running cargo clippy for $RustTarget..."
    cargo clippy --all-targets --target $RustTarget -- -D warnings -W clippy::pedantic -W clippy::nursery
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo clippy failed." }
    Write-Info "Clippy passed."
    exit 0
}

if ($CargoUpdate) {
    Write-Info "Running cargo update..."
    if ($CargoUpdateArgs -ne "") {
        $updateArgs = $CargoUpdateArgs -split '\s+'
        cargo update @updateArgs
    }
    else {
        cargo update
    }
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo update failed." }
    Write-Info "Cargo update complete."
    exit 0
}

if ($Coverage) {
    if (-not (Get-Command cargo-llvm-cov -ErrorAction SilentlyContinue)) {
        Write-Info "Installing cargo-llvm-cov..."
        cargo install cargo-llvm-cov --locked
        if ($LASTEXITCODE -ne 0) { Write-Err "Failed to install cargo-llvm-cov." }
    }
    # cargo-llvm-cov requires the MSVC toolchain (LLVM source-based coverage
    # only works with the MSVC backend).  Clear the MSYS2 compiler overrides
    # so ring/cc-rs use the native MSVC tools instead of GNU ar/gcc.
    Write-Info "Clearing MSYS2 compiler overrides for MSVC coverage build..."
    $env:CC = $null
    $env:CXX = $null
    $env:AR = $null
    $env:DLLTOOL = $null
    Write-Info "Running code coverage (MSVC toolchain)..."
    cargo llvm-cov --summary-only --ignore-filename-regex '(ui/|jellyfin/|plex/|subsonic/|radio/|db/migration|desktop_integration/|main\.rs)'
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo llvm-cov failed." }
    Write-Info "Coverage complete."
    exit 0
}

# ── Per-package dependency checks ────────────────────────────────────────────
$pkgConfig = Join-Path $MsysPath "bin\pkg-config.exe"

# Compile-time libraries (hard fail)
$requiredPkgs = @(
    @{ pc = "gtk4"; pkg = "gtk4" },
    @{ pc = "libadwaita-1"; pkg = "libadwaita" },
    @{ pc = "gstreamer-1.0"; pkg = "gstreamer" }
)

$missing = @()
foreach ($dep in $requiredPkgs) {
    $null = & $pkgConfig --exists $dep.pc 2>$null; $ok = $LASTEXITCODE -eq 0
    if ($ok) {
        Write-Host "  [ok] $($dep.pc)"
    }
    else {
        Write-Host "  [MISSING] $($dep.pc)" -ForegroundColor Red
        $missing += "$PkgPrefix-$($dep.pkg)"
    }
}
if ($missing.Count -gt 0) {
    Write-Err "Missing compile-time packages. In MSYS2 shell, run:`n  pacman -S $($missing -join ' ')"
}

# Runtime GStreamer plugins (warn only)
$gstPluginDir = Join-Path $MsysPath "lib\gstreamer-1.0"
$pluginWarnings = @()
foreach ($plugin in @("gst-plugins-good", "gst-plugins-bad", "gst-libav")) {
    $pattern = switch ($plugin) {
        "gst-plugins-good" { "libgstaudioparsers.dll" }
        "gst-plugins-bad" { "libgstfdkaac.dll" }
        "gst-libav" { "libgstlibav.dll" }
    }
    $probe = Join-Path $gstPluginDir $pattern
    if (Test-Path $probe) {
        Write-Host "  [ok] $plugin"
    }
    else {
        Write-Host "  [MISSING] $plugin (audio codecs)" -ForegroundColor Yellow
        $pluginWarnings += "$PkgPrefix-$plugin"
    }
}
if ($pluginWarnings.Count -gt 0) {
    Write-Warn "Missing GStreamer codec plugins.`n  pacman -S $($pluginWarnings -join ' ')"
}

Write-Info "All dependency checks passed."

# ── Rust Build ───────────────────────────────────────────────────────────────
if (-not $NoCargoBuild) {
    Write-Info "Building Tributary (release) for $RustTarget..."
    cargo build --release --target $RustTarget
}
else {
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

# Helper: copy a single file only if the destination doesn't exist or the
# source is newer.  This avoids re-copying hundreds of unchanged DLLs on
# every build, saving significant time on incremental rebuilds.
function Copy-IfNewer {
    param([string]$Src, [string]$Dst)
    if (-not (Test-Path $Dst)) {
        Copy-Item $Src $Dst
        return $true
    }
    $srcTime = (Get-Item $Src).LastWriteTimeUtc
    $dstTime = (Get-Item $Dst).LastWriteTimeUtc
    if ($srcTime -gt $dstTime) {
        Copy-Item $Src $Dst -Force
        return $true
    }
    return $false
}

# Helper: recursively sync a directory tree, copying only newer files.
function Sync-Directory {
    param([string]$SrcDir, [string]$DstDir)
    $copied = 0
    Get-ChildItem -Path $SrcDir -Recurse -File | ForEach-Object {
        $relPath = $_.FullName.Substring($SrcDir.Length)
        $destFile = Join-Path $DstDir $relPath
        $destDir = Split-Path $destFile
        if (-not (Test-Path $destDir)) { New-Item -ItemType Directory -Force $destDir | Out-Null }
        if (Copy-IfNewer $_.FullName $destFile) { $copied++ }
    }
    return $copied
}

New-Item -ItemType Directory -Force $DIST | Out-Null
New-Item -ItemType Directory -Force "$DIST\lib" | Out-Null

# Always copy the executable (just built).
Copy-Item $exePath $DIST -Force

# Copy dynamic plugin folders (incremental — only newer files).
Write-Info "Syncing GTK plugins and GStreamer codecs (incremental)..."
$totalCopied = 0

$loadersSrc = Join-Path $MsysPath "lib\gdk-pixbuf-2.0"
if (Test-Path $loadersSrc) {
    $n = Sync-Directory $loadersSrc (Join-Path $DIST "lib\gdk-pixbuf-2.0")
    $totalCopied += $n
}

$gstPluginSrc = Join-Path $MsysPath "lib\gstreamer-1.0"
if (Test-Path $gstPluginSrc) {
    $n = Sync-Directory $gstPluginSrc (Join-Path $DIST "lib\gstreamer-1.0")
    $totalCopied += $n
}

# Resolve all transitive dependencies for the EXE and Plugins.
# Use the explicit MSYS2 path for ldd to ensure it exists in PowerShell.
$ldd = Join-Path $Msys2Root "usr\bin\ldd.exe"
if (-not (Test-Path $ldd)) { $ldd = "ldd" }

Write-Info "Resolving required DLLs for executable and plugins..."

# Gather the exe and every single plugin dll we just copied.
$binariesToScan = @(Join-Path $DIST (Split-Path $exePath -Leaf))
$binariesToScan += Get-ChildItem -Path "$DIST\lib" -Recurse -Filter *.dll | Select-Object -ExpandProperty FullName

foreach ($bin in $binariesToScan) {
    & $ldd $bin 2>$null | ForEach-Object {
        # Extract JUST the DLL filename from the left side of the `=>` operator.
        if ($_ -match "^\s*(.+?\.dll)\s+=>") {
            $dllName = $matches[1].Trim()
            $srcPath = Join-Path $MsysPath "bin\$dllName"
            
            # If it exists in the MSYS2 bin folder, copy only if newer or missing.
            if (Test-Path $srcPath) {
                $destPath = Join-Path $DIST $dllName
                if (Copy-IfNewer $srcPath $destPath) {
                    Write-Host "  copied: $dllName"
                    $totalCopied++
                }
            }
        }
    }
}

Write-Info "Incremental sync: $totalCopied file(s) updated."

# ── GTK Resources (incremental) ──────────────────────────────────────────────
Write-Info "Syncing GTK icons and schemas (incremental)..."

foreach ($theme in @("hicolor", "Adwaita")) {
    $src = Join-Path $MsysPath "share\icons\$theme"
    $dest = Join-Path $DIST   "share\icons\$theme"
    if (Test-Path $src) {
        $n = Sync-Directory $src $dest
        $totalCopied += $n
    }
}

# Bundle the app's own hicolor icons (About dialog, etc.)
$appIconsSrc = "data\icons\hicolor"
if (Test-Path $appIconsSrc) {
    $appIconsDest = Join-Path $DIST "share\icons\hicolor"
    $n = Sync-Directory (Resolve-Path $appIconsSrc).Path $appIconsDest
    $totalCopied += $n
    Write-Info "Bundled app icons: $n file(s) synced."
}

$schemasSrc = Join-Path $MsysPath "share\glib-2.0\schemas"
$schemasDest = Join-Path $DIST   "share\glib-2.0\schemas"
if (Test-Path $schemasSrc) {
    New-Item -ItemType Directory -Force $schemasDest | Out-Null
    # Only re-copy and recompile schemas if any XML files changed.
    $schemasChanged = 0
    Get-ChildItem "$schemasSrc\*.xml" -ErrorAction SilentlyContinue | ForEach-Object {
        $destFile = Join-Path $schemasDest $_.Name
        if (Copy-IfNewer $_.FullName $destFile) { $schemasChanged++ }
    }
    if ($schemasChanged -gt 0) {
        $compiler = Join-Path $MsysPath "bin\glib-compile-schemas.exe"
        if (Test-Path $compiler) { & $compiler $schemasDest }
        $totalCopied += $schemasChanged
    }
}

Write-Info "Total incremental sync: $totalCopied file(s) updated."

# ── Zip Archive ──────────────────────────────────────────────────────────────
Write-Info "Creating zip archive..."
$zipPath = "dist\tributary-windows.zip"
Remove-Item $zipPath -ErrorAction SilentlyContinue
Compress-Archive -Path $DIST -DestinationPath $zipPath
Write-Info "Archive created: $((Get-Item $zipPath).FullName)"

# ── Inno Setup Installer (optional) ─────────────────────────────────────────
if ($InnoSetup) {
    Write-Info "Building Inno Setup installer..."

    # Determine architecture for Inno Setup
    $InnoArch = if ($env:INNO_ARCH) { $env:INNO_ARCH } else { "x64" }

    # Extract version from Cargo.toml
    $CargoVersion = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches.Groups[1].Value

    # Find iscc.exe
    $iscc = $null
    $isccPaths = @(
        "iscc.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe",
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $isccPaths) {
        if (Get-Command $p -ErrorAction SilentlyContinue) { $iscc = $p; break }
        if (Test-Path $p) { $iscc = $p; break }
    }
    if (-not $iscc) {
        Write-Err "Inno Setup compiler (iscc.exe) not found. Install Inno Setup 6 from https://jrsoftware.org/isinfo.php"
    }

    $issFile = "build-aux\inno\tributary.iss"
    $sourceDir = (Resolve-Path $DIST).Path
    $outputDir = (Resolve-Path "dist").Path

    Write-Info "Running Inno Setup compiler..."
    & $iscc /DAppVersion="$CargoVersion" /DSourceDir="$sourceDir" /DOutputDir="$outputDir" /DTargetArch="$InnoArch" $issFile
    if ($LASTEXITCODE -ne 0) { Write-Err "Inno Setup compilation failed." }

    Write-Info "Installer created: $outputDir\tributary-setup.exe"
}

Write-Info "Done."
