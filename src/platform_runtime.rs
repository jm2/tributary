//! Early platform-runtime setup for self-contained Windows and macOS builds.
//!
//! GTK and GStreamer inspect several environment variables during their first
//! initialization, so bundled paths and writable caches must be selected before
//! either toolkit is touched.  Writable registries live below the user's cache
//! directory and are separated by platform, architecture, and install path.
#![cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::bail;
#[cfg(any(test, target_os = "windows", target_os = "macos"))]
use anyhow::{anyhow, Context};

const CACHE_NAMESPACE: &str = "tributary/runtime";
#[cfg(any(test, target_os = "windows", target_os = "macos"))]
const PLATFORM_RUNTIME_PROBE_FLAG: &str = "--tributary-platform-runtime-probe";
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_PROBE_SENTINEL_NAME: &str = "tributary-platform-runtime-probe.ok";
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_PROBE_SENTINEL: &[u8] = b"tributary-windows-runtime-probe-v1\n";

#[cfg(any(test, target_os = "macos"))]
const PIXBUF_CACHE_LIMIT: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const HELPER_ERROR_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeCachePaths {
    root: PathBuf,
    gst_registry: PathBuf,
    pixbuf_loaders: PathBuf,
}

/// Configure bundled runtime paths before GTK or GStreamer initialize.
///
/// Returns `true` only when a hidden packaging probe ran successfully and the
/// process should exit without starting the normal application.
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(clippy::unnecessary_wraps)
)]
pub fn configure_before_toolkit() -> anyhow::Result<bool> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let probe_root = parse_platform_runtime_probe_request(env::args_os())?;

    #[cfg(target_os = "windows")]
    {
        configure_windows_bundle(probe_root.as_deref())
    }

    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle(probe_root.as_deref())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Ok(false)
    }
}

fn runtime_cache_paths(
    cache_base: &Path,
    platform: &str,
    architecture: &str,
    install_root: &Path,
) -> anyhow::Result<RuntimeCachePaths> {
    if !cache_base.is_absolute() {
        bail!("runtime cache base must be absolute");
    }
    if cache_base.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        bail!("runtime cache base must not contain relative path components");
    }
    if platform.is_empty() || architecture.is_empty() {
        bail!("runtime cache platform and architecture must be non-empty");
    }

    let install_key = stable_path_fingerprint(install_root);
    let root = cache_base
        .join(CACHE_NAMESPACE)
        .join(format!("{platform}-{architecture}"))
        .join(format!("{install_key:016x}"));

    Ok(RuntimeCachePaths {
        gst_registry: root.join("gstreamer").join("registry.bin"),
        pixbuf_loaders: root.join("gdk-pixbuf").join("loaders.cache"),
        root,
    })
}

fn stable_path_fingerprint(path: &Path) -> u64 {
    // FNV-1a is deliberately simple and deterministic across Rust versions.
    // This is a cache namespace, not a security boundary.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn should_set_env(existing: Option<&OsStr>) -> bool {
    existing.is_none()
}

fn should_set_gstreamer_env(unversioned: Option<&OsStr>, versioned: Option<&OsStr>) -> bool {
    should_set_env(unversioned) && should_set_env(versioned)
}

#[cfg(any(test, target_os = "macos"))]
fn needs_macos_runtime_cache(
    gst_registry: Option<&OsStr>,
    gst_registry_versioned: Option<&OsStr>,
    pixbuf_module_file: Option<&OsStr>,
) -> bool {
    should_set_gstreamer_env(gst_registry, gst_registry_versioned)
        || should_set_env(pixbuf_module_file)
}

#[cfg(target_os = "macos")]
fn set_if_unset(key: &str, value: impl AsRef<OsStr>) {
    if should_set_env(env::var_os(key).as_deref()) {
        env::set_var(key, value);
    }
}

fn set_gstreamer_if_unset(unversioned: &str, versioned: &str, value: impl AsRef<OsStr>) {
    if should_set_gstreamer_env(
        env::var_os(unversioned).as_deref(),
        env::var_os(versioned).as_deref(),
    ) {
        env::set_var(unversioned, value);
    }
}

#[cfg(target_os = "macos")]
fn cache_base() -> anyhow::Result<PathBuf> {
    dirs::cache_dir().ok_or_else(|| anyhow!("operating system did not provide a user cache path"))
}

#[cfg(target_os = "windows")]
fn configure_windows_bundle(probe_root: Option<&Path>) -> anyhow::Result<bool> {
    let exe = env::current_exe()
        .context("could not determine executable path")?
        .canonicalize()
        .context("could not resolve executable path")?;
    let layout = detect_windows_bundle(&exe);
    if probe_root.is_some() && layout.is_none() {
        bail!("platform runtime probe requires a valid Windows bundle");
    }
    let Some(layout) = layout else {
        return Ok(false);
    };

    if let Some(root) = probe_root {
        run_windows_runtime_probe(&layout, root)?;
        return Ok(true);
    }

    set_gstreamer_if_unset("GST_PLUGIN_PATH", "GST_PLUGIN_PATH_1_0", &layout.plugin_dir);
    set_gstreamer_if_unset("GST_PLUGIN_SYSTEM_PATH", "GST_PLUGIN_SYSTEM_PATH_1_0", "");
    if layout.scanner.is_file() {
        set_gstreamer_if_unset(
            "GST_PLUGIN_SCANNER",
            "GST_PLUGIN_SCANNER_1_0",
            &layout.scanner,
        );
    }

    if !should_set_gstreamer_env(
        env::var_os("GST_REGISTRY").as_deref(),
        env::var_os("GST_REGISTRY_1_0").as_deref(),
    ) {
        return Ok(false);
    }

    // If the OS has no usable user cache directory, leave this unset and
    // let GStreamer choose its normal per-user default. Never fall back
    // to a registry beside the executable.
    if let Some(registry) = prepare_windows_registry(&layout) {
        env::set_var("GST_REGISTRY", registry);
    }

    Ok(false)
}

#[cfg(target_os = "windows")]
fn prepare_windows_registry(layout: &WindowsBundleLayout) -> Option<PathBuf> {
    let base = dirs::cache_dir()?;
    let caches =
        runtime_cache_paths(&base, "windows", env::consts::ARCH, &layout.install_root).ok()?;
    ensure_cache_outside_install(&caches.root, &layout.install_root).ok()?;
    create_cache_parent(&caches.gst_registry).ok()?;
    Some(caches.gst_registry)
}

#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowsBundleLayout {
    install_root: PathBuf,
    plugin_dir: PathBuf,
    scanner: PathBuf,
}

#[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
fn detect_windows_bundle(exe: &Path) -> Option<WindowsBundleLayout> {
    let install_root = exe.parent()?.to_path_buf();
    let plugin_dir = install_root.join("lib").join("gstreamer-1.0");
    if !exe.is_file() || !plugin_dir.is_dir() {
        return None;
    }
    Some(WindowsBundleLayout {
        // Keep the scanner beside the application and its bundled DLLs. Windows
        // searches an executable's own directory for its dependencies, so this
        // works for both the isolated package probe and an ordinary user launch
        // without adding the bundle to PATH.
        scanner: install_root.join("gst-plugin-scanner.exe"),
        install_root,
        plugin_dir,
    })
}

#[cfg(target_os = "windows")]
fn run_windows_runtime_probe(
    layout: &WindowsBundleLayout,
    requested_cache_root: &Path,
) -> anyhow::Result<()> {
    reject_inherited_windows_probe_environment()?;
    if !layout.scanner.is_file() {
        bail!("platform runtime probe requires the bundled gst-plugin-scanner.exe");
    }

    let cache_base = validate_probe_cache_root(requested_cache_root, &layout.install_root)?;
    let caches = runtime_cache_paths(
        &cache_base,
        "windows",
        env::consts::ARCH,
        &layout.install_root,
    )?;
    ensure_cache_outside_install(&caches.root, &layout.install_root)?;
    create_cache_parent(&caches.gst_registry)?;

    set_exact_windows_probe_environment(layout, &caches.gst_registry);
    preflight_windows_plugin_scanner(&layout.scanner)?;
    crate::audio::run_packaged_windows_runtime_probe(&layout.plugin_dir)?;

    let registry = std::fs::metadata(&caches.gst_registry).with_context(|| {
        format!(
            "platform runtime probe registry was not created: {}",
            caches.gst_registry.display()
        )
    })?;
    if !registry.is_file() || registry.len() == 0 {
        bail!("platform runtime probe registry is empty");
    }
    if caches.gst_registry.starts_with(&layout.install_root) {
        bail!("platform runtime probe registry resolves inside the Windows bundle");
    }

    atomic_replace(
        &cache_base.join(WINDOWS_PROBE_SENTINEL_NAME),
        WINDOWS_PROBE_SENTINEL,
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn reject_inherited_windows_probe_environment() -> anyhow::Result<()> {
    let forbidden = env::vars_os().find_map(|(key, _)| {
        forbidden_windows_probe_environment_key(&key)
            .then_some(key.to_string_lossy().to_ascii_uppercase())
    });
    if let Some(key) = forbidden {
        bail!("platform runtime probe requires inherited {key} to be unset");
    }
    Ok(())
}

#[cfg(any(test, target_os = "windows"))]
fn forbidden_windows_probe_environment_key(key: &OsStr) -> bool {
    let normalized = key.to_string_lossy().to_ascii_uppercase();
    normalized.starts_with("GST_")
        || normalized == "GIO_EXTRA_MODULES"
        || normalized == "GIO_USE_PROXY_RESOLVER"
}

#[cfg(target_os = "windows")]
fn set_exact_windows_probe_environment(layout: &WindowsBundleLayout, registry: &Path) {
    for key in ["GST_PLUGIN_PATH", "GST_PLUGIN_PATH_1_0"] {
        env::set_var(key, &layout.plugin_dir);
    }
    for key in ["GST_PLUGIN_SYSTEM_PATH", "GST_PLUGIN_SYSTEM_PATH_1_0"] {
        env::set_var(key, "");
    }
    for key in ["GST_PLUGIN_SCANNER", "GST_PLUGIN_SCANNER_1_0"] {
        env::set_var(key, &layout.scanner);
    }
    for key in ["GST_REGISTRY", "GST_REGISTRY_1_0"] {
        env::set_var(key, registry);
    }
}

#[cfg(target_os = "windows")]
fn preflight_windows_plugin_scanner(scanner: &Path) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(scanner)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow!("bundled GStreamer plugin scanner could not start"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if scanner_no_args_exit_code_is_expected(status.code()) {
                    return Ok(());
                }
                bail!("bundled GStreamer plugin scanner returned an unexpected status");
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                terminate_windows_scanner(&mut child)?;
                bail!("bundled GStreamer plugin scanner exceeded its 5-second deadline");
            }
            Err(_) => {
                terminate_windows_scanner(&mut child)?;
                bail!("bundled GStreamer plugin scanner status could not be inspected");
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn terminate_windows_scanner(child: &mut std::process::Child) -> anyhow::Result<()> {
    let killed = child.kill().is_ok();
    let waited = child.wait().is_ok();
    if !killed || !waited {
        bail!("bundled GStreamer plugin scanner could not be terminated");
    }
    Ok(())
}

#[cfg(any(test, target_os = "windows"))]
fn scanner_no_args_exit_code_is_expected(code: Option<i32>) -> bool {
    code == Some(1)
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct MacBundleLayout {
    app_root: PathBuf,
    contents_dir: PathBuf,
    macos_dir: PathBuf,
    resources_dir: PathBuf,
}

#[cfg(any(test, target_os = "macos"))]
fn detect_macos_bundle(exe: &Path) -> Option<MacBundleLayout> {
    let macos_dir = exe.parent()?;
    let contents_dir = macos_dir.parent()?;
    let app_root = contents_dir.parent()?;
    if macos_dir.file_name()? != "MacOS"
        || contents_dir.file_name()? != "Contents"
        || app_root.extension()? != "app"
        || !exe.is_file()
        || !contents_dir.join("Info.plist").is_file()
        || !contents_dir.join("Resources").is_dir()
    {
        return None;
    }

    let plist = std::fs::read(contents_dir.join("Info.plist")).ok()?;
    if plist.len() > 1024 * 1024 {
        return None;
    }
    let plist = std::str::from_utf8(&plist).ok()?;
    if !plist.contains("<key>CFBundlePackageType</key>")
        || !plist.contains("<string>APPL</string>")
        || !plist.contains("<key>CFBundleExecutable</key>")
    {
        return None;
    }

    Some(MacBundleLayout {
        app_root: app_root.to_path_buf(),
        contents_dir: contents_dir.to_path_buf(),
        macos_dir: macos_dir.to_path_buf(),
        resources_dir: contents_dir.join("Resources"),
    })
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle(probe_root: Option<&Path>) -> anyhow::Result<bool> {
    let exe = env::current_exe()
        .context("could not determine executable path")?
        .canonicalize()
        .context("could not resolve executable path")?;
    let layout = detect_macos_bundle(&exe);

    if probe_root.is_some() && layout.is_none() {
        bail!("platform runtime probe requires a valid .app bundle");
    }
    let Some(layout) = layout else {
        return Ok(false);
    };

    let needs_cache = needs_macos_runtime_cache(
        env::var_os("GST_REGISTRY").as_deref(),
        env::var_os("GST_REGISTRY_1_0").as_deref(),
        env::var_os("GDK_PIXBUF_MODULE_FILE").as_deref(),
    );
    let caches = if needs_cache {
        let cache_base = if let Some(root) = probe_root {
            validate_probe_cache_root(root, &layout.app_root)?
        } else {
            cache_base()?
        };
        let paths = runtime_cache_paths(&cache_base, "macos", env::consts::ARCH, &layout.app_root)?;
        ensure_cache_outside_install(&paths.root, &layout.app_root)?;
        Some(paths)
    } else {
        None
    };

    configure_macos_environment(&layout, caches.as_ref())?;

    if probe_root.is_some() {
        let caches = caches
            .as_ref()
            .ok_or_else(|| anyhow!("platform runtime probe did not select user cache paths"))?;
        run_macos_runtime_probe(&layout, caches)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn configure_macos_environment(
    layout: &MacBundleLayout,
    caches: Option<&RuntimeCachePaths>,
) -> anyhow::Result<()> {
    let share_dir = layout.resources_dir.join("share");
    let schemas_dir = share_dir.join("glib-2.0").join("schemas");
    let gst_plugins = layout.resources_dir.join("lib").join("gstreamer-1.0");
    let gst_scanner = layout.macos_dir.join("gst-plugin-scanner");

    set_if_unset("XDG_DATA_DIRS", &share_dir);
    set_if_unset("GTK_DATA_PREFIX", &layout.resources_dir);
    set_if_unset("GSETTINGS_SCHEMA_DIR", &schemas_dir);
    set_if_unset("GTK_PATH", layout.resources_dir.join("lib").join("gtk-4.0"));
    set_gstreamer_if_unset("GST_PLUGIN_PATH", "GST_PLUGIN_PATH_1_0", &gst_plugins);
    set_gstreamer_if_unset("GST_PLUGIN_SYSTEM_PATH", "GST_PLUGIN_SYSTEM_PATH_1_0", "");
    if gst_scanner.is_file() {
        set_gstreamer_if_unset("GST_PLUGIN_SCANNER", "GST_PLUGIN_SCANNER_1_0", &gst_scanner);
    }

    if should_set_gstreamer_env(
        env::var_os("GST_REGISTRY").as_deref(),
        env::var_os("GST_REGISTRY_1_0").as_deref(),
    ) {
        let caches =
            caches.ok_or_else(|| anyhow!("GStreamer user cache path was not configured"))?;
        create_cache_parent(&caches.gst_registry)?;
        env::set_var("GST_REGISTRY", &caches.gst_registry);
    }

    if should_set_env(env::var_os("GDK_PIXBUF_MODULE_FILE").as_deref()) {
        let caches =
            caches.ok_or_else(|| anyhow!("GDK-Pixbuf user cache path was not configured"))?;
        let loader_dir = layout
            .resources_dir
            .join("lib")
            .join("gdk-pixbuf-2.0")
            .join("2.10.0")
            .join("loaders");
        let query_helper = layout.macos_dir.join("gdk-pixbuf-query-loaders");
        let loaders = bundled_pixbuf_loaders(&loader_dir)?;
        let output = run_bounded_helper(&query_helper, &loader_dir, &loaders)?;
        let validated =
            validate_pixbuf_cache_output(&output, &layout.contents_dir, &loader_dir, &loaders)?;
        atomic_replace(&caches.pixbuf_loaders, validated.as_bytes())?;
        env::set_var("GDK_PIXBUF_MODULE_FILE", &caches.pixbuf_loaders);
    }

    Ok(())
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn parse_platform_runtime_probe_request(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> anyhow::Result<Option<PathBuf>> {
    let mut args = args.into_iter().skip(1);
    let mut probe_root = None;
    while let Some(arg) = args.next() {
        if arg == PLATFORM_RUNTIME_PROBE_FLAG {
            if probe_root.is_some() {
                bail!("platform runtime probe flag may only be supplied once");
            }
            let root = args
                .next()
                .ok_or_else(|| anyhow!("platform runtime probe requires an explicit cache root"))?;
            if root == PLATFORM_RUNTIME_PROBE_FLAG {
                bail!("platform runtime probe flag may only be supplied once");
            }
            probe_root = Some(PathBuf::from(root));
        }
    }
    Ok(probe_root)
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn validate_probe_cache_root(root: &Path, app_root: &Path) -> anyhow::Result<PathBuf> {
    if !root.is_absolute() {
        bail!("platform runtime probe cache root must be absolute");
    }
    if root.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        bail!("platform runtime probe cache root must not contain relative components");
    }
    let resolved_app = app_root
        .canonicalize()
        .context("could not resolve platform runtime probe app bundle")?;
    let projected_root = resolve_existing_prefix(root)?;
    if projected_root.starts_with(&resolved_app) {
        bail!("platform runtime probe cache root must be outside the app bundle");
    }
    if root.exists() {
        if !root.is_dir() {
            bail!("platform runtime probe cache root must be a directory");
        }
        if std::fs::read_dir(root)
            .with_context(|| {
                format!(
                    "could not inspect platform runtime probe cache root {}",
                    root.display()
                )
            })?
            .next()
            .is_some()
        {
            bail!("platform runtime probe cache root must be fresh and empty");
        }
    }
    std::fs::create_dir_all(root).with_context(|| {
        format!(
            "could not create platform runtime probe cache root {}",
            root.display()
        )
    })?;
    let resolved_root = root
        .canonicalize()
        .context("could not resolve platform runtime probe cache root")?;
    if resolved_root.starts_with(&resolved_app) {
        bail!("platform runtime probe cache root must be outside the app bundle");
    }
    if std::fs::read_dir(&resolved_root)
        .with_context(|| {
            format!(
                "could not inspect platform runtime probe cache root {}",
                resolved_root.display()
            )
        })?
        .next()
        .is_some()
    {
        bail!("platform runtime probe cache root must be fresh and empty");
    }
    Ok(resolved_root)
}

#[cfg(target_os = "macos")]
fn bundled_pixbuf_loaders(loader_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(loader_dir).with_context(|| {
        format!(
            "could not read pixbuf loader directory {}",
            loader_dir.display()
        )
    })?;
    let mut loaders = Vec::new();
    for entry in entries {
        let path = entry
            .context("could not inspect bundled pixbuf loader entry")?
            .path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "so" || extension == "dylib")
        {
            loaders.push(path);
        }
    }
    loaders.sort();
    if loaders.is_empty() {
        bail!("bundled pixbuf loader directory contains no loader modules");
    }
    Ok(loaders)
}

#[cfg(target_os = "macos")]
fn run_bounded_helper(
    helper: &Path,
    loader_dir: &Path,
    loaders: &[PathBuf],
) -> anyhow::Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if !helper.is_file() {
        bail!("bundled gdk-pixbuf-query-loaders helper is missing");
    }
    let mut child = Command::new(helper)
        .args(loaders)
        .env("GDK_PIXBUF_MODULEDIR", loader_dir)
        .env_remove("GDK_PIXBUF_MODULE_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not launch bundled gdk-pixbuf-query-loaders")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("pixbuf helper stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("pixbuf helper stderr was not captured"))?;

    let stdout_thread = std::thread::spawn(move || drain_capped(stdout, PIXBUF_CACHE_LIMIT));
    let stderr_thread = std::thread::spawn(move || drain_capped(stderr, HELPER_ERROR_LIMIT));

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("could not poll pixbuf helper status")?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("pixbuf helper exceeded its 15-second deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_overflowed) = stdout_thread
        .join()
        .map_err(|_| anyhow!("pixbuf helper stdout reader panicked"))??;
    let (stderr, stderr_overflowed) = stderr_thread
        .join()
        .map_err(|_| anyhow!("pixbuf helper stderr reader panicked"))??;
    if stdout_overflowed {
        bail!("pixbuf helper output exceeded the configured limit");
    }
    if stderr_overflowed {
        bail!("pixbuf helper error output exceeded the configured limit");
    }
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr);
        bail!("pixbuf helper failed: {message}");
    }
    Ok(stdout)
}

#[cfg(target_os = "macos")]
fn drain_capped(mut reader: impl std::io::Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut stored = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = std::io::Read::read(&mut reader, &mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(stored.len());
        let kept = remaining.min(count);
        stored.extend_from_slice(&chunk[..kept]);
        overflowed |= kept < count;
    }
    Ok((stored, overflowed))
}

#[cfg(any(test, target_os = "macos"))]
fn validate_pixbuf_cache_output(
    output: &[u8],
    helper_toplevel: &Path,
    loader_dir: &Path,
    expected_loaders: &[PathBuf],
) -> anyhow::Result<String> {
    if output.is_empty() {
        bail!("pixbuf helper returned an empty cache");
    }
    if output.len() > PIXBUF_CACHE_LIMIT {
        bail!("pixbuf helper output exceeded the configured limit");
    }
    if output.contains(&0) {
        bail!("pixbuf helper output contains a NUL byte");
    }
    let text = std::str::from_utf8(output).context("pixbuf helper output is not UTF-8")?;

    let expected = expected_loaders
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut emitted = std::collections::HashSet::new();
    let mut normalized = String::with_capacity(text.len());
    #[derive(Clone, Copy)]
    enum RecordState {
        Module,
        Metadata(u8),
        Signatures,
    }
    let mut state = RecordState::Module;
    for segment in text.split_inclusive('\n') {
        let (line, line_ending) = if let Some(line) = segment.strip_suffix("\r\n") {
            (line, "\r\n")
        } else if let Some(line) = segment.strip_suffix('\n') {
            (line, "\n")
        } else {
            (segment, "")
        };
        let trimmed = line.trim();
        match state {
            RecordState::Module if trimmed.is_empty() || trimmed.starts_with('#') => {
                if let Some(declared_dir) = line.strip_prefix("# LoaderDir = ") {
                    if Path::new(declared_dir) != loader_dir {
                        bail!("pixbuf cache declares an unexpected loader directory");
                    }
                }
                normalized.push_str(line);
            }
            RecordState::Module => {
                let module_path = standalone_quoted_value(trimmed)?
                    .ok_or_else(|| anyhow!("pixbuf cache contains a malformed module record"))?;
                let declared = PathBuf::from(module_path);
                let resolved = if declared.is_absolute() {
                    declared
                } else {
                    if declared
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                    {
                        bail!("pixbuf cache contains an unsafe relative module path");
                    }
                    helper_toplevel.join(declared)
                };
                if resolved.parent() != Some(loader_dir) || !expected.contains(&resolved) {
                    let name = resolved
                        .file_name()
                        .and_then(OsStr::to_str)
                        .unwrap_or("<unprintable>");
                    let safe_name = name
                        .chars()
                        .flat_map(char::escape_default)
                        .take(128)
                        .collect::<String>();
                    bail!("pixbuf cache contains an unexpected module: {safe_name}");
                }
                if !emitted.insert(resolved.clone()) {
                    bail!("pixbuf cache contains a duplicate module path");
                }
                normalized.push('"');
                normalized.push_str(&glib_escape_path(&resolved)?);
                normalized.push('"');
                state = RecordState::Metadata(0);
            }
            RecordState::Metadata(index) => {
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    bail!("pixbuf cache contains an incomplete loader metadata record");
                }
                normalized.push_str(line);
                state = if index == 2 {
                    RecordState::Signatures
                } else {
                    RecordState::Metadata(index + 1)
                };
            }
            RecordState::Signatures => {
                normalized.push_str(line);
                if trimmed.is_empty() {
                    state = RecordState::Module;
                }
            }
        }
        normalized.push_str(line_ending);
    }
    if !matches!(state, RecordState::Module) {
        bail!("pixbuf cache ended inside an incomplete loader record");
    }
    if emitted.len() != expected.len() || expected.iter().any(|path| !emitted.contains(path)) {
        bail!("pixbuf cache omitted one or more bundled loader modules");
    }
    Ok(normalized)
}

#[cfg(any(test, target_os = "macos"))]
fn standalone_quoted_value(line: &str) -> anyhow::Result<Option<String>> {
    let bytes = line.as_bytes();
    if bytes.first() != Some(&b'"') {
        return Ok(None);
    }

    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 2;
            }
            b'"' if index == bytes.len() - 1 => {
                return decode_glib_escaped(&line[1..index]).map(Some);
            }
            b'"' => return Ok(None),
            _ => index += 1,
        }
    }
    bail!("pixbuf cache contains an unterminated quoted record")
}

#[cfg(any(test, target_os = "macos"))]
fn decode_glib_escaped(value: &str) -> anyhow::Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            if bytes[index] == b'"' {
                bail!("pixbuf cache contains an unescaped quote");
            }
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }

        index += 1;
        let escaped = *bytes
            .get(index)
            .ok_or_else(|| anyhow!("pixbuf cache contains a trailing escape"))?;
        match escaped {
            b'b' => decoded.push(0x08),
            b'f' => decoded.push(0x0c),
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'v' => decoded.push(0x0b),
            b'\\' | b'"' => decoded.push(escaped),
            b'0'..=b'7' => {
                let mut octal = u32::from(escaped - b'0');
                let mut digits = 1;
                while digits < 3
                    && bytes
                        .get(index + 1)
                        .is_some_and(|byte| matches!(byte, b'0'..=b'7'))
                {
                    index += 1;
                    octal = octal * 8 + u32::from(bytes[index] - b'0');
                    digits += 1;
                }
                let byte = u8::try_from(octal)
                    .map_err(|_| anyhow!("pixbuf cache contains an invalid octal escape"))?;
                if byte == 0 {
                    bail!("pixbuf cache contains a NUL escape");
                }
                decoded.push(byte);
            }
            _ => bail!("pixbuf cache contains an unsupported escape"),
        }
        index += 1;
    }
    String::from_utf8(decoded).context("pixbuf cache module path is not UTF-8")
}

#[cfg(any(test, target_os = "macos"))]
fn glib_escape_path(path: &Path) -> anyhow::Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| anyhow!("pixbuf loader path is not UTF-8"))?;
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            0x08 => escaped.push_str("\\b"),
            0x0c => escaped.push_str("\\f"),
            b'\n' => escaped.push_str("\\n"),
            b'\r' => escaped.push_str("\\r"),
            b'\t' => escaped.push_str("\\t"),
            0x0b => escaped.push_str("\\v"),
            b'\\' => escaped.push_str("\\\\"),
            b'"' => escaped.push_str("\\\""),
            0x01..=0x1f | 0x7f..=0xff => {
                use std::fmt::Write;
                write!(escaped, "\\{byte:03o}").expect("writing to a String cannot fail");
            }
            _ => escaped.push(char::from(byte)),
        }
    }
    Ok(escaped)
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn create_cache_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("runtime cache path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create runtime cache {}", parent.display()))
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn ensure_cache_outside_install(cache_root: &Path, install_root: &Path) -> anyhow::Result<()> {
    let projected_cache = resolve_existing_prefix(cache_root)?;
    let resolved_install = install_root
        .canonicalize()
        .with_context(|| format!("could not resolve install root {}", install_root.display()))?;
    if projected_cache.starts_with(&resolved_install) {
        bail!("runtime cache resolves inside the application install");
    }

    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("could not create runtime cache {}", cache_root.display()))?;
    let resolved_cache = cache_root
        .canonicalize()
        .with_context(|| format!("could not resolve runtime cache {}", cache_root.display()))?;
    if resolved_cache.starts_with(resolved_install) {
        bail!("runtime cache resolves inside the application install");
    }
    Ok(())
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn resolve_existing_prefix(path: &Path) -> anyhow::Result<PathBuf> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| anyhow!("runtime cache path has no existing ancestor"))?;
    }
    let resolved = existing
        .canonicalize()
        .with_context(|| format!("could not resolve cache ancestor {}", existing.display()))?;
    let suffix = path
        .strip_prefix(existing)
        .context("could not derive unresolved cache suffix")?;
    Ok(resolved.join(suffix))
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn atomic_replace(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    create_cache_parent(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("runtime cache path has no parent"))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("could not create cache temporary file")?;
    temporary
        .write_all(contents)
        .context("could not write cache temporary file")?;
    temporary
        .flush()
        .context("could not flush cache temporary file")?;
    temporary
        .as_file()
        .sync_all()
        .context("could not sync cache temporary file")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("could not atomically replace runtime cache")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_macos_runtime_probe(
    layout: &MacBundleLayout,
    caches: &RuntimeCachePaths,
) -> anyhow::Result<()> {
    let icon = layout
        .resources_dir
        .join("share/icons/hicolor/128x128/apps/io.github.tributary.Tributary.png");
    let pixbuf = gdk_pixbuf::Pixbuf::from_file(&icon).with_context(|| {
        format!(
            "bundled app icon did not load through GDK-Pixbuf: {}",
            icon.display()
        )
    })?;
    if pixbuf.width() <= 0 || pixbuf.height() <= 0 {
        bail!("bundled app icon decoded to an invalid size");
    }

    gstreamer::init().context("GStreamer initialization failed during bundle probe")?;
    if gstreamer::ElementFactory::find("playbin3").is_none() {
        bail!("required bundled GStreamer playbin3 factory was not discovered");
    }

    verify_probe_cache(&caches.pixbuf_loaders, &layout.resources_dir)?;
    verify_probe_cache(&caches.gst_registry, &layout.resources_dir)?;
    if bundle_contains_mutable_cache(layout) {
        bail!("runtime probe found a mutable cache inside the signed app bundle");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_probe_cache(cache: &Path, resources_dir: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(cache)
        .with_context(|| format!("runtime probe cache was not created: {}", cache.display()))?;
    if bytes.is_empty() {
        bail!("runtime probe cache is empty");
    }
    let needle = resources_dir.to_string_lossy();
    if !bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
    {
        bail!("runtime probe cache does not reference the current app bundle");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn bundle_contains_mutable_cache(layout: &MacBundleLayout) -> bool {
    layout.macos_dir.join("gst-registry.bin").exists()
        || layout
            .resources_dir
            .join("lib/gdk-pixbuf-2.0/2.10.0/loaders.cache")
            .exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::fs;

    fn write_fake_app(root: &Path) -> PathBuf {
        let contents = root.join("Tributary.app/Contents");
        let macos = contents.join("MacOS");
        fs::create_dir_all(contents.join("Resources")).unwrap();
        fs::create_dir_all(&macos).unwrap();
        fs::write(
            contents.join("Info.plist"),
            "<plist><dict><key>CFBundleExecutable</key><string>Tributary</string>\
             <key>CFBundlePackageType</key><string>APPL</string></dict></plist>",
        )
        .unwrap();
        let exe = macos.join("Tributary-bin");
        fs::write(&exe, b"binary").unwrap();
        exe
    }

    #[test]
    fn cache_paths_are_user_scoped_and_never_install_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let cache_base = temp.path().join("Library/Caches");
        let install_root = temp.path().join("Applications/Tributary.app");
        let paths = runtime_cache_paths(&cache_base, "macos", "aarch64", &install_root).unwrap();
        assert!(paths.root.starts_with(cache_base.join("tributary")));
        assert!(!paths.gst_registry.starts_with(&install_root));
        assert!(!paths.pixbuf_loaders.starts_with(&install_root));
        assert_eq!(
            paths.gst_registry.file_name(),
            Some(OsStr::new("registry.bin"))
        );
    }

    #[test]
    fn cache_paths_separate_platform_architecture_and_install_path() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("cache");
        let first_install = temp.path().join("A/App.app");
        let moved_install = temp.path().join("B/App.app");
        let mac_arm = runtime_cache_paths(&base, "macos", "aarch64", &first_install).unwrap();
        let mac_x64 = runtime_cache_paths(&base, "macos", "x86_64", &first_install).unwrap();
        let win_arm = runtime_cache_paths(&base, "windows", "aarch64", &first_install).unwrap();
        let moved = runtime_cache_paths(&base, "macos", "aarch64", &moved_install).unwrap();
        assert_ne!(mac_arm.root, mac_x64.root);
        assert_ne!(mac_arm.root, win_arm.root);
        assert_ne!(mac_arm.root, moved.root);
        assert!(runtime_cache_paths(
            &base.join("../redirect"),
            "macos",
            "aarch64",
            &first_install
        )
        .is_err());
    }

    #[test]
    fn explicit_environment_values_are_preserved_even_when_empty() {
        assert!(should_set_env(None));
        assert!(!should_set_env(Some(OsStr::new(""))));
        assert!(!should_set_env(Some(OsStr::new("/custom/cache"))));
        assert!(should_set_gstreamer_env(None, None));
        assert!(!should_set_gstreamer_env(Some(OsStr::new("")), None));
        assert!(!should_set_gstreamer_env(None, Some(OsStr::new(""))));
        assert!(!should_set_gstreamer_env(
            Some(OsStr::new("/custom")),
            Some(OsStr::new("/versioned"))
        ));
        assert!(!needs_macos_runtime_cache(
            Some(OsStr::new("")),
            None,
            Some(OsStr::new(""))
        ));
        assert!(!needs_macos_runtime_cache(
            None,
            Some(OsStr::new("/versioned")),
            Some(OsStr::new("/pixbuf"))
        ));
        assert!(needs_macos_runtime_cache(None, None, None));
    }

    #[test]
    fn platform_probe_parser_accepts_one_explicit_cache_root() {
        let args = [
            OsString::from("tributary"),
            OsString::from("--unrelated"),
            OsString::from(PLATFORM_RUNTIME_PROBE_FLAG),
            OsString::from("/fresh cache"),
        ];
        assert_eq!(
            parse_platform_runtime_probe_request(args).unwrap(),
            Some(PathBuf::from("/fresh cache"))
        );
        assert_eq!(
            parse_platform_runtime_probe_request([OsString::from("tributary")]).unwrap(),
            None
        );
    }

    #[test]
    fn platform_probe_parser_rejects_missing_or_duplicate_roots() {
        for args in [
            vec![
                OsString::from("tributary"),
                OsString::from(PLATFORM_RUNTIME_PROBE_FLAG),
            ],
            vec![
                OsString::from("tributary"),
                OsString::from(PLATFORM_RUNTIME_PROBE_FLAG),
                OsString::from(PLATFORM_RUNTIME_PROBE_FLAG),
            ],
            vec![
                OsString::from("tributary"),
                OsString::from(PLATFORM_RUNTIME_PROBE_FLAG),
                OsString::from("/first"),
                OsString::from(PLATFORM_RUNTIME_PROBE_FLAG),
                OsString::from("/second"),
            ],
        ] {
            assert!(parse_platform_runtime_probe_request(args).is_err());
        }
    }

    #[test]
    fn platform_probe_cache_root_must_be_fresh_empty_and_external() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Tributary.app");
        fs::create_dir(&install).unwrap();
        let cache = temp.path().join("Fresh Cache With Spaces");

        let resolved = validate_probe_cache_root(&cache, &install).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.is_dir());
        fs::write(cache.join("stale-registry.bin"), b"stale").unwrap();
        assert!(validate_probe_cache_root(&cache, &install).is_err());

        let inside = install.join("probe-cache");
        assert!(validate_probe_cache_root(&inside, &install).is_err());
        assert!(!inside.exists());
        assert!(validate_probe_cache_root(Path::new("relative-cache"), &install).is_err());
    }

    #[test]
    fn windows_probe_rejects_all_gstreamer_and_gio_policy_keys() {
        for key in [
            "GST_REGISTRY",
            "gst_debug",
            "GST_PLUGIN_LOADING_WHITELIST",
            "GIO_EXTRA_MODULES",
            "gio_use_proxy_resolver",
        ] {
            assert!(forbidden_windows_probe_environment_key(OsStr::new(key)));
        }
        for key in [
            "PATH",
            "HTTP_PROXY",
            "GIO_MODULE_DIR",
            "GDK_PIXBUF_MODULE_FILE",
        ] {
            assert!(!forbidden_windows_probe_environment_key(OsStr::new(key)));
        }
    }

    #[test]
    fn windows_scanner_preflight_accepts_only_documented_no_args_status() {
        assert!(scanner_no_args_exit_code_is_expected(Some(1)));
        for code in [None, Some(0), Some(2), Some(i32::MAX)] {
            assert!(!scanner_no_args_exit_code_is_expected(code));
        }
    }

    #[test]
    fn windows_scanner_preflight_is_bounded_silent_and_precedes_gstreamer() {
        let source = include_str!("platform_runtime.rs");
        let configure = source
            .find("set_exact_windows_probe_environment(layout, &caches.gst_registry);")
            .expect("exact probe environment");
        let preflight_call = source[configure..]
            .find("preflight_windows_plugin_scanner(&layout.scanner)?;")
            .expect("scanner preflight call")
            + configure;
        let gstreamer_probe = source[preflight_call..]
            .find("crate::audio::run_packaged_windows_runtime_probe(&layout.plugin_dir)?;")
            .expect("GStreamer probe call")
            + preflight_call;
        assert!(configure < preflight_call);
        assert!(preflight_call < gstreamer_probe);

        let preflight = source
            .split_once("fn preflight_windows_plugin_scanner")
            .expect("scanner preflight implementation")
            .1
            .split_once("fn terminate_windows_scanner")
            .expect("scanner preflight boundary")
            .0;
        for fragment in [
            ".stdin(Stdio::null())",
            ".stdout(Stdio::null())",
            ".stderr(Stdio::null())",
            "Duration::from_secs(5)",
            "child.try_wait()",
            "terminate_windows_scanner(&mut child)?",
        ] {
            assert!(preflight.contains(fragment));
        }
        assert!(!preflight.contains(".arg("));

        let termination = source
            .split_once("fn terminate_windows_scanner")
            .expect("scanner termination implementation")
            .1
            .split_once("fn scanner_no_args_exit_code_is_expected")
            .expect("scanner termination boundary")
            .0;
        assert!(termination.contains("child.kill()"));
        assert!(termination.contains("child.wait()"));
    }

    #[test]
    fn mac_bundle_detection_requires_complete_exact_shape() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_fake_app(temp.path());
        assert!(detect_macos_bundle(&exe).is_some());

        fs::remove_file(temp.path().join("Tributary.app/Contents/Info.plist")).unwrap();
        assert!(detect_macos_bundle(&exe).is_none());
    }

    #[test]
    fn suffix_only_and_false_app_shapes_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("Fake.app/NotContents/MacOS");
        fs::create_dir_all(&fake).unwrap();
        let exe = fake.join("Tributary-bin");
        fs::write(&exe, b"binary").unwrap();
        assert!(detect_macos_bundle(&exe).is_none());

        let exe = write_fake_app(temp.path());
        fs::write(
            temp.path().join("Tributary.app/Contents/Info.plist"),
            "<plist><dict><key>CFBundleExecutable</key><string>Tributary</string></dict></plist>",
        )
        .unwrap();
        assert!(detect_macos_bundle(&exe).is_none());
    }

    #[test]
    fn windows_bundle_detection_requires_executable_and_plugin_directory() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("tributary.exe");
        fs::write(&exe, b"binary").unwrap();
        assert!(detect_windows_bundle(&exe).is_none());
        fs::create_dir_all(temp.path().join("lib/gstreamer-1.0")).unwrap();
        let layout = detect_windows_bundle(&exe).unwrap();
        assert_eq!(layout.scanner, temp.path().join("gst-plugin-scanner.exe"));
    }

    fn cache_record(module_record: &str) -> String {
        use std::fmt::Write;

        let mut cache = String::new();
        writeln!(cache, "\"{module_record}\"").unwrap();
        writeln!(cache, "\"png\" 5 \"gdk-pixbuf\" \"image/png\" \"LGPL\"").unwrap();
        writeln!(cache, "\"\"").unwrap();
        writeln!(cache, "\"\"").unwrap();
        writeln!(cache).unwrap();
        cache
    }

    fn cache_text(loaders: &[PathBuf]) -> String {
        let mut cache = String::new();
        for loader in loaders {
            cache.push_str(&cache_record(&glib_escape_path(loader).unwrap()));
        }
        cache
    }

    #[test]
    fn pixbuf_cache_validation_accepts_only_exact_absolute_loaders() {
        let temp = tempfile::tempdir().unwrap();
        let loader_dir = temp
            .path()
            .join("Tributary.app/Contents/Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders");
        let loaders = vec![
            loader_dir.join("libpixbufloader-png.dylib"),
            loader_dir.join("libpixbufloader-svg.dylib"),
        ];
        let text = cache_text(&loaders);
        assert_eq!(
            validate_pixbuf_cache_output(text.as_bytes(), temp.path(), &loader_dir, &loaders)
                .unwrap(),
            text
        );
    }

    #[test]
    fn pixbuf_cache_validation_rewrites_exact_relocatable_records_to_absolute_paths() {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp.path().join("Tributáry.app/Contents");
        let loader_dir = contents.join("Resources/lib/gdk-pixbuf-2.0/2.10.0/loaders");
        let loaders = vec![
            loader_dir.join("libpixbufloader-png.dylib"),
            loader_dir.join("libpixbufloader-svg.dylib"),
        ];
        let mut relocatable = String::new();
        for loader in &loaders {
            let relative = loader.strip_prefix(&contents).unwrap();
            relocatable.push_str(&cache_record(&glib_escape_path(relative).unwrap()));
        }

        assert_eq!(
            validate_pixbuf_cache_output(relocatable.as_bytes(), &contents, &loader_dir, &loaders)
                .unwrap(),
            cache_text(&loaders)
        );
    }

    #[test]
    fn pixbuf_cache_validation_rejects_malicious_or_incomplete_output() {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp.path().join("Tributary.app/Contents");
        let loader_dir = contents.join("Resources/loaders");
        let loader = loader_dir.join("libpixbufloader-png.dylib");
        let loaders = vec![loader.clone()];
        let outside = temp.path().join("outside/libpixbufloader-png.dylib");
        let mut extra_module = cache_text(&loaders);
        extra_module.push_str(&cache_record(&glib_escape_path(&outside).unwrap()));
        for malicious in [
            String::new(),
            cache_record("relative-loader.dylib"),
            cache_record(&glib_escape_path(&outside).unwrap()),
            extra_module,
            cache_record("../libpixbufloader-png.dylib"),
        ] {
            assert!(validate_pixbuf_cache_output(
                malicious.as_bytes(),
                &contents,
                &loader_dir,
                &loaders
            )
            .is_err());
        }
        assert!(
            validate_pixbuf_cache_output(&[0xff, 0xfe], &contents, &loader_dir, &loaders).is_err()
        );
    }

    #[test]
    fn atomic_replace_replaces_old_cache_from_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("nested/loaders.cache");
        atomic_replace(&cache, b"old").unwrap();
        atomic_replace(&cache, b"new").unwrap();
        assert_eq!(fs::read(&cache).unwrap(), b"new");
        assert_eq!(fs::read_dir(cache.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_cache_never_replaces_last_known_good_cache() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("loaders.cache");
        atomic_replace(&cache, b"known-good").unwrap();
        let helper_toplevel = temp.path().join("bundle");
        let loader_dir = helper_toplevel.join("loaders");
        let loaders = vec![loader_dir.join("loader.dylib")];
        let invalid = b"\"/tmp/attacker.dylib\"\n";
        assert!(
            validate_pixbuf_cache_output(invalid, &helper_toplevel, &loader_dir, &loaders).is_err()
        );
        assert_eq!(fs::read(cache).unwrap(), b"known-good");
    }

    #[test]
    fn cache_parent_creation_failure_is_reported_without_install_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("not-a-directory");
        fs::write(&blocker, b"file").unwrap();
        let cache = blocker.join("registry.bin");
        assert!(atomic_replace(&cache, b"cache").is_err());
        assert!(!temp.path().join("registry.bin").exists());
    }

    #[test]
    fn cache_root_inside_install_is_rejected_before_creation() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Tributary.app");
        fs::create_dir(&install).unwrap();
        let cache = install.join("Contents/new-runtime-cache");
        assert!(ensure_cache_outside_install(&cache, &install).is_err());
        assert!(!cache.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cache_parent_into_install_is_rejected_before_creation() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Tributary.app");
        fs::create_dir(&install).unwrap();
        let cache_link = temp.path().join("user-cache");
        symlink(&install, &cache_link).unwrap();
        let cache = cache_link.join("nested/runtime");
        assert!(ensure_cache_outside_install(&cache, &install).is_err());
        assert!(!install.join("nested").exists());
    }

    #[test]
    fn macos_script_orders_cache_removal_signing_probe_and_final_verify() {
        let script = include_str!("../scripts/build-macos.sh");
        let remove_pixbuf = script
            .find("rm -f \"$PIXBUF_CACHE\"")
            .expect("pixbuf cache removal");
        let sign_bundle = script
            .find("codesign --force --deep --sign - \"$APP_BUNDLE\"")
            .expect("bundle signing");
        let probe = script
            .find("--tributary-platform-runtime-probe \"$PROBE_CACHE\"")
            .expect("runtime probe");
        let final_verify = script
            .rfind("codesign --verify --deep --strict --verbose=2 \"$APP_BUNDLE\"")
            .expect("final strict verification");
        assert!(remove_pixbuf < sign_bundle);
        assert!(sign_bundle < probe);
        assert!(probe < final_verify);

        let after_sign = &script[sign_bundle..];
        assert!(!after_sign.contains("rm -f \"${APP_BUNDLE}"));
        assert!(!after_sign.contains("sed -i ''"));
        let after_final_verify = &script[final_verify..];
        assert!(!after_final_verify.contains("chmod -R a-w \"$APP_BUNDLE\""));
        assert!(!after_final_verify.contains("rm -rf \"$APP_BUNDLE\""));
    }

    #[test]
    fn macos_script_bundles_fixes_and_signs_pixbuf_helper() {
        let script = include_str!("../scripts/build-macos.sh");
        assert!(script.contains(
            "PIXBUF_QUERY_DEST=\"${APP_BUNDLE}/Contents/MacOS/gdk-pixbuf-query-loaders\""
        ));
        assert!(script.contains("cp \"$PIXBUF_QUERY_SRC\" \"$PIXBUF_QUERY_DEST\""));
        assert!(script.contains("fix_rpaths \"$PIXBUF_QUERY_DEST\""));
        assert!(script.contains("codesign --force --sign - \"$PIXBUF_QUERY_DEST\""));
        assert!(script.contains("PROBE_PARENT=\"dist/Tributary Runtime Probe With Spaces\""));
        assert!(script.contains("chmod -R a-w \"$PROBE_APP\""));
        assert!(!script.contains("-print -quit"));
        for variable in [
            "GST_REGISTRY_1_0",
            "GST_PLUGIN_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GST_PLUGIN_SCANNER_1_0",
        ] {
            assert!(script.contains(&format!("-u {variable}")));
        }
        assert!(!script.contains("export GST_REGISTRY="));
        assert!(!script.contains("export GDK_PIXBUF_MODULE_FILE="));
    }

    #[test]
    fn windows_script_overwrites_and_dependency_scans_bundled_scanner() {
        let script = include_str!("../scripts/build-windows.ps1");
        assert!(script.contains("$gstScannerDest = Join-Path $DIST \"gst-plugin-scanner.exe\""));
        assert!(!script.contains(
            "$gstScannerDest = Join-Path $DIST \"libexec\\gstreamer-1.0\\gst-plugin-scanner.exe\""
        ));
        let scanner_copy = script
            .find("Copy-Item -LiteralPath $gstScannerSrc -Destination $gstScannerDest -Force")
            .expect("unconditional scanner copy");
        let scanner_scan = script
            .find("$initialDllScanTargets += $gstScannerDest")
            .expect("scanner dependency scan");
        let runtime_probe = script
            .find("Write-Info \"Running packaged Windows runtime probe...\"")
            .expect("packaged runtime probe");
        let archive = script
            .find("Write-Info \"Creating zip archive...\"")
            .expect("zip creation");
        assert!(scanner_copy < scanner_scan);
        assert!(scanner_scan < runtime_probe);
        assert!(runtime_probe < archive);
        assert!(!script.contains("Copy-IfNewer $gstScannerSrc"));
        assert!(script.contains(
            "Remove-Item -LiteralPath $legacyGstScannerDest -Force -ErrorAction SilentlyContinue"
        ));
    }

    #[test]
    fn windows_script_computes_a_bounded_nonexecuting_pe_import_closure() {
        let script = include_str!("../scripts/build-windows.ps1");
        for fragment in [
            "$requiredSoupPluginName = \"libgstsoup.dll\"",
            "$requiredSoupRuntimeName = \"libsoup-3.0-0.dll\"",
            "Required souphttpsrc runtime is incomplete",
            "$PkgPrefix-libsoup3 packages",
            "function Get-PeImportDependencyName",
            "Name: libfoo.dll",
            "--coff-imports includes ordinary and delay-load imports",
            "if ($Line -notmatch '^\\s*Name:\\s*([^\\\\/:*?\"<>|\\x00-\\x1F]+\\.dll)\\s*$')",
            "$peImportInspector = [System.IO.Path]::GetFullPath((Join-Path $MsysPath \"bin\\llvm-readobj.exe\"))",
            "[System.IO.Path]::IsPathRooted($peImportInspector)",
            "Required PE import inspector not found",
            "$PkgPrefix-llvm package",
            "function Invoke-BoundedPeImportBatch",
            "PE import-inspection target must be an absolute existing DLL or EXE",
            "Start-Process -FilePath $Inspector -ArgumentList $arguments",
            "-RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath",
            "while (-not $process.WaitForExit(50))",
            "[System.IO.File]::ReadAllLines($stdoutPath)",
            "Stop-BoundedProcessTree $process \"PE import inspector\"",
            "$maxDllScanTargets = 4096",
            "$maxPeInspectorOutputLines = 131072",
            "$maxPeInspectorBatchTargets = 28",
            "$maxPeInspectorArgumentCharacters = 24000",
            "$maxPeInspectorBatchOutputBytes = 8388608",
            "$peInspectorBatchDeadlineMs = 45000",
            "$peInspectorClosureDeadlineMs = 300000",
            "$dllScanQueue = [System.Collections.Queue]::new()",
            "$knownDllScanTargets = @{}",
            "Invoke-BoundedPeImportBatch `",
            "-Paths @($requiredSoupPluginFull)",
            "$scannedDllTargets[$requiredSoupPluginFull] = $true",
            "while ($dllScanQueue.Count -gt 0)",
            "$roundTargets = @()",
            "$batchTargets.Count -lt $maxPeInspectorBatchTargets",
            "Add-PeImportDependencies $batchLines $batchNames",
            "Add-DllScanTarget $Queue $Known $destPath $TargetLimit",
            "if ($ClosureClock.ElapsedMilliseconds -ge $ClosureDeadlineMs)",
            "Unresolved DLL import $dllName reported for $SourceLabel",
            "$systemPath = Join-Path ([System.Environment]::SystemDirectory) $dllName",
            "$isApiSet = $dllName -match '^(?i:api-ms-win-|ext-ms-win-)'",
            "$soupRuntimeDependencyObserved = $true",
            "if (-not $soupPluginScanned)",
            "if (-not $soupRuntimeDependencyObserved)",
            "$scannedDllTargets.ContainsKey($requiredSoupRuntimeFull)",
        ] {
            assert!(
                script.contains(fragment),
                "missing bounded Windows PE-import closure contract: {fragment}"
            );
        }

        let copy = script
            .find("if (Copy-IfNewer $srcPath $destPath)")
            .expect("dependency copy");
        let enqueue = script[copy..]
            .find("Add-DllScanTarget $Queue $Known $destPath")
            .expect("copied dependency enqueue")
            + copy;
        let probe = script
            .find("Write-Info \"Running packaged Windows runtime probe...\"")
            .expect("packaged runtime probe");
        assert!(copy < enqueue);
        assert!(enqueue < probe);
        assert!(!script.contains("foreach ($bin in $binariesToScan)"));
        assert!(!script.contains("Get-ChildItem -Path \"$MsysPath\\bin\""));
        assert!(!script.contains("Get-LddDependencyName"));
        assert!(!script.contains("$ldd"));
        assert!(!script.contains("ReadToEnd"));
        assert!(!script.contains("DataReceived"));
    }

    #[test]
    fn windows_pe_import_batches_use_explicit_parameter_binding() {
        let script = include_str!("../scripts/build-windows.ps1");
        assert_eq!(
            script.matches("Invoke-BoundedPeImportBatch `").count(),
            2,
            "every production PE-import batch call must be covered"
        );
        assert_eq!(script.matches("-Inspector $peImportInspector `").count(), 2);
        assert_eq!(
            script
                .matches("-ClosureClock $peInspectorClosureClock `")
                .count(),
            2
        );
        assert_eq!(
            script
                .matches("-ClosureDeadlineMs $peInspectorClosureDeadlineMs `")
                .count(),
            2
        );
        assert_eq!(
            script
                .matches("-ProcessDeadlineMs $peInspectorBatchDeadlineMs `")
                .count(),
            2
        );
        assert_eq!(
            script
                .matches("-OutputByteLimit $maxPeInspectorBatchOutputBytes `")
                .count(),
            2
        );
        assert_eq!(
            script
                .matches("-ArgumentCharacterLimit $maxPeInspectorArgumentCharacters)")
                .count(),
            2
        );
        assert_eq!(
            script
                .matches("-Paths @($requiredSoupPluginFull) `")
                .count(),
            1
        );
        assert_eq!(script.matches("-Paths $batchTargets `").count(), 1);
        assert!(
            !script.contains("$peImportInspector @($requiredSoupPluginFull)"),
            "a non-terminal string-array parameter must not rely on positional binding"
        );
    }

    #[test]
    fn windows_script_resolves_the_relative_distribution_before_import_inspection() {
        let script = include_str!("../scripts/build-windows.ps1");
        let create = script
            .find("New-Item -ItemType Directory -Force $DIST | Out-Null")
            .expect("relative distribution creation");
        let resolve = script
            .find("$DIST = (Resolve-Path -LiteralPath $DIST).ProviderPath")
            .expect("physical provider distribution resolution");
        let soup_inspection = script
            .find("Write-Host \"  inspecting required Soup plugin imports...\"")
            .expect("required Soup inspection");

        assert!(script.contains("$DIST = \"dist\\tributary-windows\""));
        assert!(create < resolve);
        assert!(resolve < soup_inspection);
        assert!(!script[resolve..soup_inspection].contains("$DIST = \"dist\\tributary-windows\""));
        for fragment in [
            "$gstScannerDest = Join-Path $DIST \"gst-plugin-scanner.exe\"",
            "$requiredSoupPluginDest = Join-Path $DIST",
            "$initialDllScanTargets = @(Join-Path $DIST",
            "$initialDllScanTargets += Get-ChildItem -Path \"$DIST\\lib\"",
            "$requiredSoupRuntimeDest = Join-Path $DIST",
        ] {
            let target = script
                .find(fragment)
                .unwrap_or_else(|| panic!("missing distribution-rooted PE target: {fragment}"));
            assert!(
                resolve < target,
                "PE target was constructed before distribution resolution: {fragment}"
            );
        }
    }

    #[test]
    fn powershell_provider_resolution_handles_changed_pwd_spaces_and_psdrives() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("Tributary Repository With Spaces");
        fs::create_dir(&repository).unwrap();
        let command = r#"
$ErrorActionPreference = "Stop"
Set-Location -LiteralPath $env:TRIBUTARY_PATH_TEST_REPOSITORY
$DIST = "dist\tributary-windows"
$legacyFullPath = [System.IO.Path]::GetFullPath($DIST)
New-Item -ItemType Directory -Force $DIST | Out-Null
$DIST = (Resolve-Path -LiteralPath $DIST).ProviderPath
$expected = Join-Path (Get-Location).Path "dist\tributary-windows"
if ($DIST -ne $expected) { throw "distribution did not resolve against PowerShell PWD" }
if (-not [System.IO.Path]::IsPathRooted($DIST)) { throw "distribution is not absolute" }
if (-not (Test-Path -LiteralPath $DIST -PathType Container)) { throw "distribution does not exist" }
if ($DIST -eq $legacyFullPath) { throw "test did not separate process cwd from PowerShell PWD" }

$driveName = "TributaryPathTest"
New-PSDrive -Name $driveName -PSProvider FileSystem -Root $env:TRIBUTARY_PATH_TEST_REPOSITORY | Out-Null
try {
    Set-Location -LiteralPath "${driveName}:\"
    $DIST = "dist\tributary-windows"
    New-Item -ItemType Directory -Force $DIST | Out-Null
    $resolved = Resolve-Path -LiteralPath $DIST
    $providerPath = $resolved.ProviderPath
    $drivePath = $resolved.Path
    $expected = Join-Path $env:TRIBUTARY_PATH_TEST_REPOSITORY "dist\tributary-windows"
    if ($providerPath -ne $expected) { throw "provider path did not resolve to the physical repository" }
    if (-not [System.IO.Path]::IsPathRooted($providerPath)) { throw "provider path is not absolute" }
    if (-not (Test-Path -LiteralPath $providerPath -PathType Container)) { throw "provider path does not exist" }
    if ($drivePath -eq $providerPath) { throw "test did not exercise a custom FileSystem PSDrive" }
}
finally {
    Set-Location -LiteralPath $env:TRIBUTARY_PATH_TEST_REPOSITORY
    Remove-PSDrive -Name $driveName
}
"#;

        let run = |program: &str| {
            std::process::Command::new(program)
                .args(["-NoProfile", "-NonInteractive", "-Command", command])
                .current_dir(temp.path())
                .env("TRIBUTARY_PATH_TEST_REPOSITORY", &repository)
                .output()
        };
        let output = match run("pwsh") {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match run("powershell") {
                Ok(output) => output,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => panic!("could not run Windows PowerShell path regression: {error}"),
            },
            Err(error) => panic!("could not run PowerShell path regression: {error}"),
        };

        assert!(
            output.status.success(),
            "PowerShell path regression failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn windows_script_runs_a_sanitized_deadline_bounded_exact_probe() {
        let script = include_str!("../scripts/build-windows.ps1");
        for fragment in [
            "$startInfo.ArgumentList.Add(\"--tributary-platform-runtime-probe\")",
            "$startInfo.ArgumentList.Add($probeCache)",
            "$startInfo.PSObject.Properties.Name -contains \"ArgumentList\"",
            "$probeCache.IndexOf([char]34)",
            "$startInfo.Arguments = \"--tributary-platform-runtime-probe `\"$probeCache`\"\"",
            "Fresh Cache With Spaces",
            "$startInfo.EnvironmentVariables.Keys",
            "$startInfo.EnvironmentVariables.Remove($key)",
            "$normalized.StartsWith(\"GST_\")",
            "$normalized -eq \"GIO_EXTRA_MODULES\"",
            "$normalized -eq \"GIO_USE_PROXY_RESOLVER\"",
            "'^(HTTP|HTTPS|ALL|NO)_PROXY$'",
            "$system32 = [System.Environment]::SystemDirectory",
            "$startInfo.EnvironmentVariables[\"PATH\"] = $system32",
            "$probeOutputLimit = 1MB",
            "$probeProcess.WaitForExit(50)",
            "$probeClock.ElapsedMilliseconds -ge 90000",
            "($stdoutLength + $stderrLength) -gt $probeOutputLimit",
            "$useTaskkill = $null -eq $killTreeMethod",
            "$killTreeMethod.Invoke($Process, [object[]]@($true))",
            "$taskkillPath = Join-Path $system32 \"taskkill.exe\"",
            "[System.IO.Path]::IsPathRooted($taskkillPath)",
            "$taskkillInfo.Arguments = \"/PID $($Process.Id) /T /F\"",
            "$taskkillProcess.WaitForExit(10000)",
            "try { $Process.Kill() } catch { }",
            "$Process.WaitForExit(10000)",
            "Get-BoundedProbeDiagnostic",
            "Remove-Item -LiteralPath $probeWorkspace -Recurse -Force",
        ] {
            assert!(
                script.contains(fragment),
                "missing Windows probe policy: {fragment}"
            );
        }
        assert!(!script.contains(
            "$startInfo.EnvironmentVariables[\"PATH\"] = \"$distFull$([System.IO.Path]::PathSeparator)$system32\""
        ));
        assert!(!script.contains("$startInfo.Environment["));
        assert!(!script.contains("$Process.Kill($true)"));
        assert!(script.contains(WINDOWS_PROBE_SENTINEL_NAME));
        assert!(script.contains(
            std::str::from_utf8(WINDOWS_PROBE_SENTINEL)
                .unwrap()
                .trim_end()
        ));
    }

    #[test]
    fn windows_script_supports_windows_powershell_5_1_process_apis() {
        let script = include_str!("../scripts/build-windows.ps1");
        for fragment in [
            "$startInfo.PSObject.Properties.Name -contains \"ArgumentList\"",
            "$probeCache.IndexOf([char]34)",
            "$startInfo.Arguments = \"--tributary-platform-runtime-probe `\"$probeCache`\"\"",
            "$startInfo.EnvironmentVariables.Keys",
            "$startInfo.EnvironmentVariables.Remove($key)",
            "$killTreeMethod.Invoke($Process, [object[]]@($true))",
            "$taskkillPath = Join-Path $system32 \"taskkill.exe\"",
            "[System.IO.Path]::IsPathRooted($taskkillPath)",
            "$taskkillInfo.Arguments = \"/PID $($Process.Id) /T /F\"",
            "$taskkillProcess.WaitForExit(10000)",
            "$Process.WaitForExit(10000)",
        ] {
            assert!(
                script.contains(fragment),
                "missing Windows PowerShell 5.1 compatibility contract: {fragment}"
            );
        }
        assert!(!script.contains("$startInfo.Environment["));
        assert!(!script.contains("$Process.Kill($true)"));
    }

    #[test]
    fn windows_script_documents_skip_bundle_installer_probe_boundary() {
        let script = include_str!("../scripts/build-windows.ps1");
        assert!(script.contains("existing, already-probed"));
        assert!(script.contains("bundle/runtime probe skipped"));
        let installer_only = script
            .find("if ($InnoSetup -and $SkipBundle)")
            .expect("installer-only mode");
        let dependency_checks = script
            .find("Write-Info \"Checking build dependencies...\"")
            .expect("dependency checks");
        assert!(installer_only < dependency_checks);
    }

    #[test]
    fn native_windows_workflows_probe_before_publishing_each_bundle() {
        let ci = include_str!("../.github/workflows/ci.yml");
        let ci_bundle = ci
            .find("pwsh -File scripts/build-windows.ps1 -Msys2Root \"$MSYS_ROOT\" -NoCargoBuild")
            .expect("CI native Windows bundle invocation");
        let ci_upload = ci[ci_bundle..]
            .find("name: Upload Windows zip")
            .expect("CI Windows upload")
            + ci_bundle;
        assert!(ci_bundle < ci_upload);
        assert!(!ci[ci_bundle..ci_upload].contains("-SkipBundle"));

        let release = include_str!("../.github/workflows/release.yml");
        let release_bundle = release
            .find("pwsh -File scripts/build-windows.ps1 -Msys2Root \"$MSYS_ROOT\" -NoCargoBuild")
            .expect("release native Windows bundle invocation");
        let release_rename = release[release_bundle..]
            .find("name: Rename zip for release")
            .expect("release zip rename")
            + release_bundle;
        let installer_only = release[release_rename..]
            .find("-NoCargoBuild -SkipBundle -InnoSetup")
            .expect("downstream release installer-only invocation")
            + release_rename;
        let release_upload = release[installer_only..]
            .find("name: Upload Windows artifacts")
            .expect("release Windows upload")
            + installer_only;
        assert!(release_bundle < release_rename);
        assert!(!release[release_bundle..release_rename].contains("-SkipBundle"));
        assert!(release_rename < installer_only);
        assert!(installer_only < release_upload);
    }
}
