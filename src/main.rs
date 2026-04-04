//! Tributary — A high-performance, Rhythmbox-style media manager.
//!
//! This is the application entry point. It initialises the tracing
//! subsystem, spawns a tokio background runtime for async I/O,
//! creates the GTK4/libadwaita application, and hands off to the
//! UI builder on activation.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[allow(dead_code)]
mod architecture;
mod audio;
#[allow(dead_code)]
mod daap;
mod db;
mod desktop_integration;
mod discovery;
#[allow(dead_code)]
mod jellyfin;
#[allow(dead_code)]
mod local;
#[allow(dead_code)]
mod platform;
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
        // extremely laggy.  Request the Vulkan renderer instead — GTK4
        // will automatically fall back through ngl → gl → cairo if
        // Vulkan is unavailable.  Only override if the user hasn't
        // already set GSK_RENDERER (so power users can still force a
        // specific renderer).
        if std::env::var_os("GSK_RENDERER").is_none() {
            std::env::set_var("GSK_RENDERER", "vulkan");
        }
    }

    // ── macOS .app bundle environment setup ──────────────────────────
    // When launched from a .app bundle (e.g. Finder / Launchpad), the
    // working directory is unpredictable and LSEnvironment relative
    // paths don't resolve correctly.  Detect the bundle at runtime and
    // set absolute paths so GTK/Adwaita can find icons, schemas, etc.
    #[cfg(target_os = "macos")]
    setup_macos_bundle_env();

    // ── Tracing ──────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tributary=info".into()),
        )
        .init();

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
                .application_icon("tributary")
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
                // Development: <repo>/target/release/tributary → <repo>/data
                if let Some(repo) = exe
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                {
                    let data_dir = repo.join("data");
                    if data_dir.is_dir() {
                        icon_theme.add_search_path(&data_dir);
                    }
                }
                // Installed / bundled: exe next to data/
                if let Some(dir) = exe.parent() {
                    let data_dir = dir.join("data");
                    if data_dir.is_dir() {
                        icon_theme.add_search_path(&data_dir);
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

    let exe = match env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    // Canonicalise symlinks so we get the real path inside the bundle.
    let exe = exe.canonicalize().unwrap_or(exe);

    // Check we're inside a .app bundle:
    //   …/Tributary.app/Contents/MacOS/Tributary
    let macos_dir = match exe.parent() {
        Some(d) => d,
        None => return,
    };
    let contents_dir = match macos_dir.parent() {
        Some(d) => d,
        None => return,
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
