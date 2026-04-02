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

    // Keep the runtime alive on a background thread
    let _rt_thread = thread::spawn(move || {
        rt.block_on(async {
            // Park the runtime until the process exits
            tokio::signal::ctrl_c().await.ok();
        });
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
    app.add_action_entries([quit_action]);
    // <primary> = Cmd on macOS, Ctrl on Linux/Windows.
    app.set_accels_for_action("app.quit", &["<primary>q"]);

    app.connect_activate(move |app| {
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
