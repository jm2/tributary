<#
.SYNOPSIS
    Tributary — Windows release build helper

.DESCRIPTION
    Requires: MSYS2, Rust, cargo in PATH. Compiles the Rust application and 
    bundles it with all required GTK4/MSYS2 DLLs and assets into a zip file.

.PARAMETER Msys2Root
    The root directory of the MSYS2 installation. Defaults to "C:\msys64".

.PARAMETER SkipBundle
    If specified by itself, just compiles the binary without DLL bundling,
    the packaged-runtime probe, or zip creation. With -InnoSetup, skips the
    build/bundle/probe steps and creates an installer from the existing dist
    folder; that folder must therefore come from an already-probed bundle run.

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

.PARAMETER Test
    If specified, sets up the MSYS2 build environment and runs `cargo test --release`.
    Useful for running the test suite from PowerShell without a full build.

.PARAMETER Run
    If specified, sets up the MSYS2 build environment, builds in release mode, and
    runs the compiled binary. Useful for quick launch from PowerShell.

.PARAMETER Coverage
    If specified, sets up the MSYS2 build environment and runs `cargo llvm-cov`
    with the project's curated --ignore-filename-regex.  Installs cargo-llvm-cov
    if it is not already present.

.PARAMETER CargoUpdate
    If specified, sets up the MSYS2 build environment and runs `cargo update` with
    any additional arguments passed via -CargoUpdateArgs. Useful for updating
    dependencies from PowerShell (e.g. -CargoUpdate -CargoUpdateArgs "-p rustls-webpki").

.PARAMETER Help
    Show this help and exit.  Equivalent to `Get-Help .\scripts\build-windows.ps1 -Full`.
#>
param(
    [string]$Msys2Root = "C:\msys64",
    [switch]$SkipBundle,
    [switch]$NoCargoBuild,
    [switch]$InnoSetup,
    [switch]$Check,
    [switch]$Clippy,
    [switch]$Fmt,
    [switch]$Test,
    [switch]$Run,
    [switch]$Coverage,
    [switch]$CargoUpdate,
    [string]$CargoUpdateArgs = "",
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ── --help / -Help / -? short-circuit ──────────────────────────────────────────
# Print the comment-based help and exit before any environment setup runs.
# Also accept the bash-style `--help` arg for parity with build-linux.sh /
# build-macos.sh; it arrives as an unbound positional, so we sniff $args.
if ($Help -or ($args -contains '--help') -or ($args -contains '-h')) {
    Get-Help -Full $PSCommandPath
    exit 0
}

function Write-Info { Write-Host "[tributary] $args" -ForegroundColor Green }
function Write-Warn { Write-Host "[tributary] $args" -ForegroundColor Yellow }
function Write-Err { Write-Host "[tributary] $args" -ForegroundColor Red; exit 1 }

function Get-BoundedProbeDiagnostic {
    param(
        [string]$Path,
        [string]$Label,
        [int]$Limit = 32768
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return "" }

    $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try {
        $length = $stream.Length
        $count = [int][Math]::Min([int64]$Limit, $length)
        if ($length -gt $count) { $null = $stream.Seek(-$count, [System.IO.SeekOrigin]::End) }
        $bytes = [byte[]]::new($count)
        $offset = 0
        while ($offset -lt $count) {
            $read = $stream.Read($bytes, $offset, $count - $offset)
            if ($read -eq 0) { break }
            $offset += $read
        }
        $text = [System.Text.Encoding]::UTF8.GetString($bytes, 0, $offset)
        $prefix = if ($length -gt $count) { "[earlier $Label output truncated; showing final $count bytes]`n" } else { "" }
        return "$prefix$text"
    }
    finally {
        $stream.Dispose()
    }
}

function Stop-ProbeProcessTree {
    param([System.Diagnostics.Process]$Process)
    if ($null -eq $Process -or $Process.HasExited) { return }

    # Process.Kill(bool) is unavailable in Windows PowerShell 5.1's .NET
    # Framework. Prefer it when present; otherwise use the absolute inbox
    # taskkill path so termination never depends on the sanitized PATH.
    $killTreeMethod = $Process.GetType().GetMethods() |
        Where-Object {
            $_.Name -eq "Kill" -and
            $_.GetParameters().Count -eq 1 -and
            ($_.GetParameters())[0].ParameterType -eq [bool]
        } |
        Select-Object -First 1

    $useTaskkill = $null -eq $killTreeMethod
    if (-not $useTaskkill) {
        try {
            $null = $killTreeMethod.Invoke($Process, [object[]]@($true))
        }
        catch {
            # The process may have won the race and exited between HasExited
            # and Invoke. Otherwise fall through to the PowerShell 5.1 path.
            $useTaskkill = -not $Process.HasExited
        }
    }

    $taskkillFailure = $null
    if ($useTaskkill) {
        $system32 = [System.Environment]::SystemDirectory
        $taskkillPath = Join-Path $system32 "taskkill.exe"
        $taskkillProcess = $null
        try {
            if (-not [System.IO.Path]::IsPathRooted($taskkillPath) -or
                -not (Test-Path -LiteralPath $taskkillPath -PathType Leaf)) {
                throw "absolute System32 taskkill.exe was not available"
            }

            $taskkillInfo = [System.Diagnostics.ProcessStartInfo]::new()
            $taskkillInfo.FileName = $taskkillPath
            $taskkillInfo.Arguments = "/PID $($Process.Id) /T /F"
            $taskkillInfo.UseShellExecute = $false
            $taskkillInfo.CreateNoWindow = $true
            $taskkillProcess = [System.Diagnostics.Process]::new()
            $taskkillProcess.StartInfo = $taskkillInfo
            if (-not $taskkillProcess.Start()) {
                throw "absolute System32 taskkill.exe could not start"
            }
            if (-not $taskkillProcess.WaitForExit(10000)) {
                try { $taskkillProcess.Kill() } catch { }
                $null = $taskkillProcess.WaitForExit(1000)
                throw "absolute System32 taskkill.exe exceeded its 10-second deadline"
            }
            if ($taskkillProcess.ExitCode -ne 0 -and -not $Process.HasExited) {
                throw "absolute System32 taskkill.exe could not terminate the probe tree"
            }
        }
        catch {
            $taskkillFailure = $_.Exception.Message
            # This cannot guarantee descendant cleanup, but it prevents the
            # packaged application itself from being orphaned if taskkill is
            # unavailable or fails unexpectedly.
            if (-not $Process.HasExited) {
                try { $Process.Kill() } catch { }
            }
        }
        finally {
            if ($null -ne $taskkillProcess) { $taskkillProcess.Dispose() }
        }
    }

    if (-not $Process.WaitForExit(10000)) {
        throw "packaged runtime probe process tree did not terminate within 10 seconds"
    }
    if ($taskkillFailure) {
        throw "packaged runtime probe required degraded termination: $taskkillFailure"
    }
}

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
# When -InnoSetup is passed with -SkipBundle, use an existing, already-probed
# dist tree and skip straight to installer creation. This intentionally does
# not claim to validate a tree that may have been changed since its bundle run.
if ($InnoSetup -and $SkipBundle) {
    Write-Info "Building Inno Setup installer from the existing dist tree (bundle/runtime probe skipped)..."

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

$cargoNeeded = (-not $NoCargoBuild) -or $Check -or $Clippy -or $Fmt -or $Test -or $Run -or $CargoUpdate
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
    Write-Info "Running cargo clippy for $RustTarget (debug --all-targets)..."
    cargo clippy --all-targets --target $RustTarget -- -D warnings
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo clippy (debug) failed." }
    Write-Info "Running cargo clippy for $RustTarget (release)..."
    cargo clippy --release --target $RustTarget -- -D warnings
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo clippy (release) failed." }
    Write-Info "Clippy passed."
    exit 0
}

if ($Test) {
    Write-Info "Running cargo test (release) for $RustTarget..."
    cargo test --release --target $RustTarget
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo test failed." }
    Write-Info "All tests passed."
    exit 0
}

if ($Run) {
    Write-Info "Building Tributary (release) for $RustTarget..."
    cargo build --release --target $RustTarget
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo build failed." }
    $runExePath = "target\$RustTarget\release\tributary.exe"
    if (-not (Test-Path $runExePath)) { Write-Err "Binary not found at $runExePath" }
    Write-Info "Launching $runExePath..."
    & $runExePath
    exit $LASTEXITCODE
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

# Runtime GStreamer plugins (Soup source required; additional codecs warn only)
$gstPluginDir = Join-Path $MsysPath "lib\gstreamer-1.0"
$requiredSoupPluginName = "libgstsoup.dll"
$requiredSoupRuntimeName = "libsoup-3.0-0.dll"
$requiredSoupPluginSrc = Join-Path $gstPluginDir $requiredSoupPluginName
$requiredSoupRuntimeSrc = Join-Path $MsysPath "bin\$requiredSoupRuntimeName"
$missingSoupRuntime = @()
if (-not (Test-Path -LiteralPath $requiredSoupPluginSrc -PathType Leaf)) {
    $missingSoupRuntime += "GStreamer Soup source plugin ($requiredSoupPluginName)"
}
if (-not (Test-Path -LiteralPath $requiredSoupRuntimeSrc -PathType Leaf)) {
    $missingSoupRuntime += "Soup HTTP runtime ($requiredSoupRuntimeName)"
}
if ($missingSoupRuntime.Count -gt 0) {
    Write-Err "Required souphttpsrc runtime is incomplete: $($missingSoupRuntime -join ', '). Install the matching $PkgPrefix-gst-plugins-good and $PkgPrefix-libsoup3 packages."
}
Write-Host "  [ok] souphttpsrc plugin and Soup runtime"

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
    if ($LASTEXITCODE -ne 0) { Write-Err "cargo build failed." }
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

# Extract one dependency basename from either common MSYS2 ldd form:
#   libfoo.dll => /clang64/bin/libfoo.dll (0x...)
#   /clangarm64/bin/foo.dll (0x...)
# Reject path separators and other invalid Windows filename characters after
# taking the leaf so ldd output can never redirect a copy outside MSYS2 bin.
function Get-LddDependencyName {
    param([string]$Line)
    $candidate = $null
    if ($Line -match '^\s*(.+?\.dll)\s*=>') {
        $candidate = $matches[1]
    }
    elseif ($Line -match '^\s*"?(.+?\.dll)"?(?:\s+\(0x[0-9A-Fa-f]+\))?\s*$') {
        $candidate = $matches[1]
    }
    if (-not $candidate) { return $null }

    $candidate = $candidate.Trim().Trim([char]34).Replace([char]92, [char]47)
    $slash = $candidate.LastIndexOf([char]47)
    $leaf = if ($slash -ge 0) { $candidate.Substring($slash + 1) } else { $candidate }
    if ($leaf -notmatch '^[^\\/:*?"<>|\x00-\x1F]+\.dll$') { return $null }
    return $leaf
}

function Add-DllScanTarget {
    param(
        [System.Collections.Queue]$Queue,
        [hashtable]$Known,
        [string]$Path,
        [int]$Limit
    )
    $fullPath = [System.IO.Path]::GetFullPath($Path)
    if ($Known.ContainsKey($fullPath)) { return }
    if ($Known.Count -ge $Limit) {
        Write-Err "DLL dependency closure exceeded its $Limit-binary safety limit."
    }
    $Known[$fullPath] = $true
    $Queue.Enqueue($fullPath)
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

# gst-plugin-scanner is a required part of the packaged GStreamer runtime.
# Always overwrite it: an incremental dist tree may have been produced for a
# different architecture, and source timestamps cannot prove binary identity.
# Keep it beside Tributary and the root-level bundled DLLs so Windows can
# resolve its dependencies during both normal launches and the isolated probe
# without adding the bundle directory to PATH.
$gstScannerSrc = Join-Path $MsysPath "libexec\gstreamer-1.0\gst-plugin-scanner.exe"
$gstScannerDest = Join-Path $DIST "gst-plugin-scanner.exe"
$legacyGstScannerDest = Join-Path $DIST "libexec\gstreamer-1.0\gst-plugin-scanner.exe"
if (-not (Test-Path -LiteralPath $gstScannerSrc -PathType Leaf)) {
    Write-Err "Required GStreamer plugin scanner not found at $gstScannerSrc"
}
Remove-Item -LiteralPath $legacyGstScannerDest -Force -ErrorAction SilentlyContinue
Copy-Item -LiteralPath $gstScannerSrc -Destination $gstScannerDest -Force
Write-Info "Bundled gst-plugin-scanner.exe (unconditional overwrite)."

# Resolve all transitive dependencies for the EXE and Plugins.
# Use the explicit MSYS2 path for ldd to ensure it exists in PowerShell.
$ldd = Join-Path $Msys2Root "usr\bin\ldd.exe"
if (-not (Test-Path $ldd)) { $ldd = "ldd" }

Write-Info "Resolving required DLLs for executable and plugins..."

# Seed the dependency closure with Tributary, every copied plugin, and the
# exact scanner. Every MSYS2 runtime DLL discovered below is copied to the
# bundle root and enqueued in turn, so transitive dependencies reach closure.
$requiredSoupPluginDest = Join-Path $DIST "lib\gstreamer-1.0\$requiredSoupPluginName"
if (-not (Test-Path -LiteralPath $requiredSoupPluginDest -PathType Leaf)) {
    Write-Err "Required souphttpsrc plugin was not copied into the Windows bundle."
}

$maxDllScanTargets = 4096
$maxLddOutputLines = 131072
$dllScanQueue = [System.Collections.Queue]::new()
$knownDllScanTargets = @{}
$scannedDllTargets = @{}
$initialDllScanTargets = @(Join-Path $DIST (Split-Path $exePath -Leaf))
$initialDllScanTargets += Get-ChildItem -Path "$DIST\lib" -Recurse -Filter *.dll | Select-Object -ExpandProperty FullName
$initialDllScanTargets += $gstScannerDest
foreach ($bin in $initialDllScanTargets) {
    Add-DllScanTarget $dllScanQueue $knownDllScanTargets $bin $maxDllScanTargets
}

$requiredSoupPluginFull = [System.IO.Path]::GetFullPath($requiredSoupPluginDest)
$requiredSoupRuntimeDest = Join-Path $DIST $requiredSoupRuntimeName
$requiredSoupRuntimeFull = [System.IO.Path]::GetFullPath($requiredSoupRuntimeDest)
$soupPluginScanned = $false
$soupRuntimeDependencyObserved = $false
$lddOutputLineCount = 0

while ($dllScanQueue.Count -gt 0) {
    $bin = [string]$dllScanQueue.Dequeue()
    if ($scannedDllTargets.ContainsKey($bin)) { continue }
    $scannedDllTargets[$bin] = $true
    $isSoupPlugin = $bin -ieq $requiredSoupPluginFull
    if ($isSoupPlugin) { $soupPluginScanned = $true }

    $lddLines = @(& $ldd $bin 2>$null)
    $lddExitCode = $LASTEXITCODE
    if ($lddExitCode -ne 0) {
        Write-Err "DLL dependency inspection failed for $([System.IO.Path]::GetFileName($bin)) (ldd status $lddExitCode)."
    }

    foreach ($line in $lddLines) {
        $lddOutputLineCount++
        if ($lddOutputLineCount -gt $maxLddOutputLines) {
            Write-Err "DLL dependency closure exceeded its $maxLddOutputLines-line safety limit."
        }

        $dllName = Get-LddDependencyName ([string]$line)
        if (-not $dllName) { continue }
        if ([string]$line -match '=>\s+not found') {
            Write-Err "Unresolved DLL dependency $dllName required by $([System.IO.Path]::GetFileName($bin))."
        }
        if ($isSoupPlugin -and $dllName -ieq $requiredSoupRuntimeName) {
            $soupRuntimeDependencyObserved = $true
        }

        # Copy only dependencies ldd identifies and that exist in the selected
        # MSYS2 architecture's bin directory; do not sweep unrelated DLLs.
        $srcPath = Join-Path $MsysPath "bin\$dllName"
        if (Test-Path -LiteralPath $srcPath -PathType Leaf) {
            $destPath = Join-Path $DIST $dllName
            if (Copy-IfNewer $srcPath $destPath) {
                Write-Host "  copied: $dllName"
                $totalCopied++
            }
            Add-DllScanTarget $dllScanQueue $knownDllScanTargets $destPath $maxDllScanTargets
        }
    }
}

if (-not $soupPluginScanned) {
    Write-Err "Required souphttpsrc plugin was not inspected by the DLL dependency closure."
}
if (-not $soupRuntimeDependencyObserved) {
    Write-Err "Required souphttpsrc plugin did not report its $requiredSoupRuntimeName dependency."
}
if (-not (Test-Path -LiteralPath $requiredSoupRuntimeDest -PathType Leaf) -or
    -not $scannedDllTargets.ContainsKey($requiredSoupRuntimeFull)) {
    Write-Err "Required souphttpsrc runtime dependency $requiredSoupRuntimeName was not copied and inspected."
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

    # Rebuild the hicolor icon-theme.cache so it includes the app icon.
    # The cache from MSYS2 only indexes system icons; without a rebuild
    # GTK cannot find io.github.tributary.Tributary via the icon theme.
    $iconCacheUpdater = Join-Path $MsysPath "bin\gtk4-update-icon-cache.exe"
    if (Test-Path $iconCacheUpdater) {
        & $iconCacheUpdater -f -t $appIconsDest 2>$null
        Write-Info "Rebuilt hicolor icon-theme.cache."
    }
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

# ── Packaged Runtime Probe ──────────────────────────────────────────────────
# Run the bundled executable itself before archiving it. The child receives no
# ambient GStreamer/GIO/proxy policy; Tributary and the scanner resolve bundled
# DLLs from their own directory while PATH contains only Windows System32. The
# probe must build a brand-new external registry in a path containing spaces.
# The Rust probe writes its sentinel only after the bundled plugin/scanner/origin
# and protected HTTP playback checks all pass.
Write-Info "Running packaged Windows runtime probe..."
$distFull = (Resolve-Path $DIST).Path
$probeExe = Join-Path $distFull (Split-Path $exePath -Leaf)
$probeWorkspace = Join-Path ([System.IO.Path]::GetTempPath()) ("Tributary Windows Runtime Probe With Spaces " + [Guid]::NewGuid().ToString("N"))
$probeCache = Join-Path $probeWorkspace "Fresh Cache With Spaces"
$probeStdout = Join-Path $probeWorkspace "stdout.log"
$probeStderr = Join-Path $probeWorkspace "stderr.log"
$probeSentinel = Join-Path $probeCache "tributary-platform-runtime-probe.ok"
$expectedSentinel = [System.Text.Encoding]::UTF8.GetBytes("tributary-windows-runtime-probe-v1`n")
$probeOutputLimit = 1MB
$probeProcess = $null
$stdoutStream = $null
$stderrStream = $null
$stdoutCopy = $null
$stderrCopy = $null
$probeFailure = $null

try {
    New-Item -ItemType Directory -Force $probeCache | Out-Null
    try {
        $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $probeExe
        $startInfo.WorkingDirectory = $distFull
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        if ($startInfo.PSObject.Properties.Name -contains "ArgumentList") {
            $startInfo.ArgumentList.Add("--tributary-platform-runtime-probe")
            $startInfo.ArgumentList.Add($probeCache)
        }
        else {
            # Windows paths cannot contain a quote, but keep the fallback
            # fail-closed if that invariant ever changes upstream.
            if ($probeCache.IndexOf([char]34) -ge 0) {
                throw "runtime probe cache path contains an unsupported quote"
            }
            $startInfo.Arguments = "--tributary-platform-runtime-probe `"$probeCache`""
        }

        # ProcessStartInfo begins with a copy of this process's environment.
        # Remove every policy input the Rust probe refuses to inherit, plus all
        # conventional proxy variables, using case-insensitive comparisons.
        foreach ($key in @($startInfo.EnvironmentVariables.Keys)) {
            $normalized = $key.ToUpperInvariant()
            if ($normalized.StartsWith("GST_") -or
                $normalized -eq "GIO_EXTRA_MODULES" -or
                $normalized -eq "GIO_USE_PROXY_RESOLVER" -or
                $normalized -match '^(HTTP|HTTPS|ALL|NO)_PROXY$') {
                $null = $startInfo.EnvironmentVariables.Remove($key)
            }
            elseif ($normalized -eq "PATH") {
                $null = $startInfo.EnvironmentVariables.Remove($key)
            }
        }
        $system32 = [System.Environment]::SystemDirectory
        $startInfo.EnvironmentVariables["PATH"] = $system32

        $probeProcess = [System.Diagnostics.Process]::new()
        $probeProcess.StartInfo = $startInfo
        $probeClock = [System.Diagnostics.Stopwatch]::StartNew()
        if (-not $probeProcess.Start()) { throw "could not start the bundled executable" }

        $stdoutStream = [System.IO.File]::Create($probeStdout)
        $stderrStream = [System.IO.File]::Create($probeStderr)
        $stdoutCopy = $probeProcess.StandardOutput.BaseStream.CopyToAsync($stdoutStream)
        $stderrCopy = $probeProcess.StandardError.BaseStream.CopyToAsync($stderrStream)

        while (-not $probeProcess.WaitForExit(50)) {
            if ($probeClock.ElapsedMilliseconds -ge 90000) {
                throw "packaged runtime probe exceeded its 90-second deadline"
            }
            $stdoutLength = $stdoutStream.Length
            $stderrLength = $stderrStream.Length
            if ($stdoutLength -gt $probeOutputLimit -or
                $stderrLength -gt $probeOutputLimit -or
                ($stdoutLength + $stderrLength) -gt $probeOutputLimit) {
                throw "packaged runtime probe output crossed its 1 MiB flood threshold"
            }
        }
        if ($probeClock.ElapsedMilliseconds -ge 90000) {
            throw "packaged runtime probe exceeded its 90-second deadline"
        }
        # Flush redirected async readers before inspecting their tasks/files.
        $probeProcess.WaitForExit()
        if ($probeProcess.ExitCode -ne 0) {
            throw "bundled executable exited with status $($probeProcess.ExitCode)"
        }
    }
    catch {
        $probeFailure = $_.Exception.Message
    }
    finally {
        # Keep process-tree termination and redirected-stream cleanup nested so
        # a timeout, kill failure, or copy failure cannot bypass final cleanup.
        try {
            Stop-ProbeProcessTree $probeProcess
        }
        catch {
            if ($probeFailure) { $probeFailure += "; $($_.Exception.Message)" }
            else { $probeFailure = $_.Exception.Message }
        }
        finally {
            try {
                $copyTasks = @($stdoutCopy, $stderrCopy) | Where-Object { $null -ne $_ }
                if ($copyTasks.Count -gt 0) {
                    if (-not [System.Threading.Tasks.Task]::WaitAll([System.Threading.Tasks.Task[]]$copyTasks, 10000)) {
                        throw "redirected output exceeded its 10-second drain deadline"
                    }
                }
            }
            catch {
                if ($probeFailure) { $probeFailure += "; redirected output did not drain: $($_.Exception.Message)" }
                else { $probeFailure = "redirected output did not drain: $($_.Exception.Message)" }
            }
            finally {
                if ($null -ne $stdoutStream) { $stdoutStream.Dispose() }
                if ($null -ne $stderrStream) { $stderrStream.Dispose() }
                if ($null -ne $probeProcess) { $probeProcess.Dispose() }
            }
        }
    }

    # Recheck after both async pipe copies drain so output written between the
    # final poll and process exit cannot evade the flood check. The async
    # files may cross the threshold before the next poll, but diagnostics are
    # always read back through the fixed-size tail helper above.
    if ((Test-Path -LiteralPath $probeStdout -PathType Leaf) -and
        (Test-Path -LiteralPath $probeStderr -PathType Leaf)) {
        $stdoutLength = (Get-Item -LiteralPath $probeStdout).Length
        $stderrLength = (Get-Item -LiteralPath $probeStderr).Length
        if ($stdoutLength -gt $probeOutputLimit -or
            $stderrLength -gt $probeOutputLimit -or
            ($stdoutLength + $stderrLength) -gt $probeOutputLimit) {
            if ($probeFailure) { $probeFailure += "; packaged runtime probe output crossed its 1 MiB flood threshold" }
            else { $probeFailure = "packaged runtime probe output crossed its 1 MiB flood threshold" }
        }
    }

    if (-not $probeFailure) {
        if (-not (Test-Path -LiteralPath $probeSentinel -PathType Leaf)) {
            $probeFailure = "bundled executable did not write the runtime-probe sentinel"
        }
        else {
            $actualSentinel = [System.IO.File]::ReadAllBytes($probeSentinel)
            if ([Convert]::ToBase64String($actualSentinel) -ne [Convert]::ToBase64String($expectedSentinel)) {
                $probeFailure = "runtime-probe sentinel content was not exact"
            }
        }
    }

    if ($probeFailure) {
        $stdoutDiagnostic = Get-BoundedProbeDiagnostic $probeStdout "stdout"
        $stderrDiagnostic = Get-BoundedProbeDiagnostic $probeStderr "stderr"
        $probeFailure += "`n--- bounded stdout ---`n$stdoutDiagnostic`n--- bounded stderr ---`n$stderrDiagnostic"
    }
}
finally {
    # Exception-safe cleanup includes the fresh cache, exact sentinel, and
    # bounded diagnostic files; no probe state is shipped in the archive.
    Remove-Item -LiteralPath $probeWorkspace -Recurse -Force -ErrorAction SilentlyContinue
}

if ($probeFailure) { Write-Err "Packaged Windows runtime probe failed: $probeFailure" }
Write-Info "Packaged Windows runtime probe passed."

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
