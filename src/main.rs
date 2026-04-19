//! Tributary — A high-performance, Rhythmbox-style media manager.
//!
//! This is the application entry point. It initialises the tracing
//! subsystem, spawns a tokio background runtime for async I/O,
//! creates the GTK4/libadwaita application, and hands off to the
//! UI builder on activation.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// ── Clippy pedantic / nursery configuration ─────────────────────────────
// Enable pedantic and nursery for deep static analysis, but selectively
// allow lints that are too noisy for a GTK application codebase.
#![warn(clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::doc_markdown,            // Too many false positives on technical terms (GLib, SQLite, etc.)
    clippy::similar_names,           // Intentional: artist_resp/artists_resp, value/value2 are clear
    clippy::too_many_lines,          // GTK UI builders are inherently long
    clippy::redundant_clone,         // GTK GObject clones are required for move closures
    clippy::wildcard_imports,        // Standard pattern for gtk::prelude::*
    clippy::cast_possible_truncation,// Deliberate u64↔i64↔u32 conversions for DB/UI interop
    clippy::cast_sign_loss,          // Deliberate i32→u32 for DB model conversions
    clippy::cast_possible_wrap,      // Deliberate u32→i32 for SeaORM compatibility
    clippy::cast_precision_loss,     // u64→f64 for progress/duration display
    clippy::cast_lossless,           // Allow explicit `as` casts for clarity
    clippy::struct_field_names,      // track_number on Track is intentional
    clippy::module_name_repetitions, // Acceptable for backend::BackendError etc.
    clippy::items_after_statements,  // Common pattern in GTK signal handler setup
    clippy::significant_drop_tightening, // False positives with GTK widget builders
    clippy::redundant_closure_for_method_calls, // Often clearer with explicit closures
    clippy::option_if_let_else,      // if-let is often clearer than map_or
    clippy::match_same_arms,         // Intentional for exhaustive match documentation
    clippy::trivially_copy_pass_by_ref, // &bool/&u32 in trait impls
    clippy::needless_pass_by_value,  // GTK signal handlers require owned values
    clippy::unreadable_literal,      // Constants like 86400, 604800 are well-known
    clippy::map_unwrap_or,           // .map().unwrap_or() is often clearer than .map_or()
    clippy::uninlined_format_args,   // format!("{}", x) vs format!("{x}") — both fine
    clippy::unnecessary_literal_bound, // &str return types in trait impls
    clippy::missing_const_for_fn,    // Many fns could be const but aren't worth marking
    clippy::assigning_clones,        // clone_from() not always clearer
    clippy::if_not_else,             // !x.is_empty() is often the natural condition
    clippy::iter_over_hash_type,     // HashSet iteration order is fine for our use cases
    clippy::ref_option,              // Option<&T> vs &Option<T> — existing API signatures
    clippy::single_match_else,       // match with _ => {} is fine for clarity
    clippy::derive_partial_eq_without_eq, // Not all PartialEq types need Eq
)]

// ── i18n initialisation ─────────────────────────────────────────────────
// Load translations from the `locales/` directory at compile time.
// English is the fallback language; all missing keys resolve to English.
rust_i18n::i18n!("locales", fallback = "en");

#[allow(dead_code)]
mod architecture;
mod audio;
#[allow(dead_code)]
mod daap;
mod db;
mod desktop_integration;
#[allow(dead_code)]
mod device;
mod discovery;
#[allow(dead_code)]
mod jellyfin;
#[allow(dead_code)]
mod local;
#[allow(dead_code)]
mod plex;
mod radio;
mod subsonic;
mod ui;

use std::sync::Arc;
use std::thread;

use adw::prelude::*;
use gtk::{gio, glib};
use tracing::info;

/// Reverse-DNS application identifier.
const APP_ID: &str = "io.github.tributary.Tributary";

fn main() {
    // ── Windows: attach to parent console ────────────────────────────
    // The `windows_subsystem = "windows"` attribute prevents a console
    // window from popping up when double-clicking the exe.  However,
    // when launched from a terminal (PowerShell, cmd), we want log
    // output to appear there.  `AttachConsole(ATTACH_PARENT_PROCESS)`
    // re-attaches to the launching terminal if one exists; it silently
    // fails when launched from Explorer (no parent console).
    #[cfg(target_os = "windows")]
    {
        extern "system" {
            fn AttachConsole(dw_process_id: u32) -> i32;
        }
        const ATTACH_PARENT_PROCESS: u32 = 0xFFFFFFFF;
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }

        // GTK4 on Windows defaults to the Cairo software renderer which
        // makes libadwaita animations (dialog slide-in, fade, blur)
        // extremely laggy.  Request the GL renderer for hardware
        // acceleration — it is more universally compatible than Vulkan
        // (which can fail on some drivers and under WSL).  Only override
        // if the user hasn't already set GSK_RENDERER (so power users
        // can still force a specific renderer).
        if std::env::var_os("GSK_RENDERER").is_none() {
            std::env::set_var("GSK_RENDERER", "gl");
        }
    }

    // ── macOS .app bundle environment setup ──────────────────────────
    // When launched from a .app bundle (e.g. Finder / Launchpad), the
    // working directory is unpredictable and LSEnvironment relative
    // paths don't resolve correctly.  Detect the bundle at runtime and
    // set absolute paths so GTK/Adwaita can find icons, schemas, etc.
    #[cfg(target_os = "macos")]
    setup_macos_bundle_env();

    // ── WSL: force GL renderer to avoid broken Vulkan/dzn ────────────
    // WSL's Dozen (dzn) Vulkan driver is often incomplete and causes
    // blank windows or rendering failures.  Detect WSL and force the
    // GL renderer which works reliably with WSLg.
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("GSK_RENDERER").is_none() {
            let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
                || std::env::var_os("WSL_INTEROP").is_some()
                || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists();
            if is_wsl {
                std::env::set_var("GSK_RENDERER", "gl");
            }
        }
    }

    // ── Tracing ──────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tributary=info".into()),
        )
        .init();

    // ── i18n: detect system locale ───────────────────────────────────
    // Normalise the system locale for rust-i18n lookup:
    // - Unify underscore separators to hyphens ("zh_CN" → "zh-CN").
    // - Try the full locale first (e.g. "zh-CN", "pt-BR") to match
    //   region-specific translation files.
    // - Fall back to the base language code (e.g. "en-US" → "en") for
    //   languages that only have a single translation file.
    let raw_locale = sys_locale::get_locale().unwrap_or_else(|| "en".to_string());
    let normalised = raw_locale.replace('_', "-");
    let available = rust_i18n::available_locales!();
    let locale = if available.contains(&normalised.as_str()) {
        normalised
    } else {
        normalised.split('-').next().unwrap_or("en").to_string()
    };
    rust_i18n::set_locale(&locale);
    info!("Locale set to: {locale} (system: {raw_locale})");

    info!("Tributary v{} starting", env!("CARGO_PKG_VERSION"));

    // ── Tokio Runtime (background thread) ────────────────────────────
    // GTK owns the main thread.  We run tokio on a dedicated thread
    // and keep a handle for spawning async tasks from signal handlers.
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime"),
    );
    let rt_handle = rt.handle().clone();

    // Keep the runtime alive on a background thread.
    // We use `std::future::pending()` to park the runtime indefinitely —
    // it will be torn down when the process exits via `std::process::exit`.
    // Using `ctrl_c` was problematic: on Windows without a console it may
    // never fire, and if it fires early it drops in-flight tasks.
    let _rt_thread = thread::spawn(move || {
        rt.block_on(std::future::pending::<()>());
    });

    // ── Library engine channel ───────────────────────────────────────
    let (engine_tx, engine_rx) = async_channel::unbounded();

    // ── GTK Application ──────────────────────────────────────────────
    glib::set_prgname(Some("Tributary"));
    glib::set_application_name("Tributary");

    let app = adw::Application::builder().application_id(APP_ID).build();

    // ── Application actions ─────────────────────────────────────────
    let quit_action = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build();

    // Register the app icon search path inside connect_activate,
    // after GTK has initialised the display.  We store the exe path
    // now so the closure can use it later.
    let exe_path = std::env::current_exe().ok();

    let about_action = gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| {
            let about = adw::AboutDialog::builder()
                .application_name("Tributary")
                .application_icon("io.github.tributary.Tributary")
                .developer_name("John-Michael Mulesa")
                .version(env!("CARGO_PKG_VERSION"))
                .website("https://github.com/jm2/tributary")
                .issue_url("https://github.com/jm2/tributary/issues")
                .copyright("© 2026 John-Michael Mulesa")
                .license_type(gtk::License::Gpl30)
                .build();

            if let Some(win) = app.active_window() {
                about.present(Some(&win));
            }
        })
        .build();

    app.add_action_entries([quit_action, about_action]);
    // <primary> = Cmd on macOS, Ctrl on Linux/Windows.
    app.set_accels_for_action("app.quit", &["<primary>q"]);

    app.connect_activate(move |app| {
        // Register the app icon search path now that GTK has a display.
        if let Some(ref exe) = exe_path {
            if let Some(display) = gtk::gdk::Display::default() {
                let icon_theme = gtk::IconTheme::for_display(&display);
                // Development: <repo>/target/release/tributary → <repo>/data/icons
                // Also handles `-Run` mode where exe is in target/<profile>/.
                if let Some(repo) = exe
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                {
                    let icons_dir = repo.join("data").join("icons");
                    if icons_dir.is_dir() {
                        icon_theme.add_search_path(&icons_dir);
                    }
                }
                // Also try two levels up (target/x86_64-pc-windows-gnullvm/release/)
                if let Some(repo) = exe
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                {
                    let icons_dir = repo.join("data").join("icons");
                    if icons_dir.is_dir() {
                        icon_theme.add_search_path(&icons_dir);
                    }
                }
                // Installed / bundled: exe next to share/icons (Windows dist)
                if let Some(dir) = exe.parent() {
                    let share_icons = dir.join("share").join("icons");
                    if share_icons.is_dir() {
                        icon_theme.add_search_path(&share_icons);
                    }
                    // Also check data/icons for Windows dist layout
                    let data_icons = dir.join("data").join("icons");
                    if data_icons.is_dir() {
                        icon_theme.add_search_path(&data_icons);
                    }
                }
                // macOS .app bundle: Contents/MacOS/Tributary-bin → Contents/Resources/share/icons
                if let Some(dir) = exe.parent() {
                    let bundle_icons = dir
                        .parent() // Contents
                        .map(|p| p.join("Resources").join("share").join("icons"));
                    if let Some(ref icons) = bundle_icons {
                        if icons.is_dir() {
                            icon_theme.add_search_path(icons);
                        }
                    }
                }
            }
        }

        ui::window::build_window(app, rt_handle.clone(), engine_tx.clone(), engine_rx.clone());
    });

    // Run the GTK main loop.  This blocks until the last window closes.
    let exit_code = app.run();
    std::process::exit(exit_code.into());
}

// ── macOS .app bundle environment ───────────────────────────────────────

/// Detect whether we are running inside a `.app` bundle and, if so,
/// set the environment variables that GTK4, libadwaita, GDK-Pixbuf,
/// and GStreamer need to find their bundled resources.
///
/// The `.app` layout is:
/// ```text
/// Tributary.app/
///   Contents/
///     MacOS/Tributary          ← executable
///     Resources/
///       share/icons/…          ← icon themes (hicolor, Adwaita)
///       share/glib-2.0/schemas ← compiled GSettings schemas
///       lib/gdk-pixbuf-2.0/…   ← pixbuf loaders
///       lib/gstreamer-1.0/…    ← GStreamer plugins
/// ```
///
/// `LSEnvironment` in `Info.plist` uses relative paths which only work
/// when the working directory happens to be `Contents/MacOS`.  macOS
/// does **not** guarantee that — Finder typically sets it to `/`.
/// This function computes absolute paths from `current_exe()`.
#[cfg(target_os = "macos")]
fn setup_macos_bundle_env() {
    use std::env;
    use std::path::PathBuf;

    let Ok(exe) = env::current_exe() else { return };

    // Canonicalise symlinks so we get the real path inside the bundle.
    let exe = exe.canonicalize().unwrap_or(exe);

    // Check we're inside a .app bundle:
    //   …/Tributary.app/Contents/MacOS/Tributary
    let Some(macos_dir) = exe.parent() else {
        return;
    };
    let Some(contents_dir) = macos_dir.parent() else {
        return;
    };

    // Verify the directory structure looks like a .app bundle.
    if !macos_dir.ends_with("Contents/MacOS") {
        return; // Not running from a .app bundle — nothing to do.
    }

    let resources_dir = contents_dir.join("Resources");
    if !resources_dir.is_dir() {
        return; // No Resources directory — probably a dev build.
    }

    // Helper: always set the var when inside a .app bundle.
    // We unconditionally override because LSEnvironment (if present in
    // Info.plist) may have set these to broken relative paths.  The
    // absolute paths we compute here are authoritative.
    let set_bundle_var = |key: &str, value: PathBuf| {
        if value.exists() {
            env::set_var(key, &value);
        }
    };

    // XDG_DATA_DIRS — GTK and Adwaita look here for icon themes.
    let share_dir = resources_dir.join("share");
    set_bundle_var("XDG_DATA_DIRS", share_dir.clone());

    // GSETTINGS_SCHEMA_DIR — compiled GSettings schemas.
    let schemas_dir = share_dir.join("glib-2.0").join("schemas");
    set_bundle_var("GSETTINGS_SCHEMA_DIR", schemas_dir);

    // GDK_PIXBUF_MODULE_FILE — pixbuf loader cache.
    let pixbuf_cache = resources_dir
        .join("lib")
        .join("gdk-pixbuf-2.0")
        .join("2.10.0")
        .join("loaders.cache");
    set_bundle_var("GDK_PIXBUF_MODULE_FILE", pixbuf_cache);

    // GST_PLUGIN_PATH — bundled GStreamer plugins.
    let gst_plugins = resources_dir.join("lib").join("gstreamer-1.0");
    set_bundle_var("GST_PLUGIN_PATH", gst_plugins.clone());

    // Prevent GStreamer from also scanning system plugin paths which
    // may contain incompatible versions.
    if gst_plugins.is_dir() {
        env::set_var("GST_PLUGIN_SYSTEM_PATH", "");
        // Force a fresh registry scan so stale system paths don't win.
        let registry = macos_dir.join("gst-registry.bin");
        env::set_var("GST_REGISTRY", &registry);
    }

    // GST_PLUGIN_SCANNER — bundled helper binary that scans plugins.
    // Without this, GStreamer uses the system's gst-plugin-scanner which
    // loads the system libgstreamer, causing duplicate ObjC class conflicts
    // (GstCocoaApplicationDelegate) and crashes.
    let gst_scanner = macos_dir.join("gst-plugin-scanner");
    if gst_scanner.is_file() {
        env::set_var("GST_PLUGIN_SCANNER", &gst_scanner);
    }

    // GTK_PATH — helps GTK find the bundled IM modules / print backends.
    let gtk_path = resources_dir.join("lib").join("gtk-4.0");
    set_bundle_var("GTK_PATH", gtk_path);
}
