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
// rust-i18n expands the complete 13-catalog literal map into one startup-only
// initializer. The catalog is intentionally complete rather than relying on
// fallback copy, so the generated initializer can exceed Clippy's conservative
// per-closure frame threshold. `initialize_i18n_backend` runs it once on a
// short-lived thread with an explicit stack before the first lookup.
#[allow(clippy::large_stack_frames)]
mod localization_catalog {
    rust_i18n::i18n!("locales", fallback = "en");

    // The parent performs the one-time force before any catalog lookup; this
    // deliberately crosses the otherwise private module boundary.
    #[allow(clippy::redundant_pub_crate)]
    pub(super) fn initialize_backend() {
        let _ = std::sync::LazyLock::force(&_RUST_I18N_BACKEND);
    }
}
use localization_catalog::*;

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
mod external_file;
pub(crate) mod http_body;
pub(crate) mod http_security;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod http_test_service;
#[allow(dead_code)]
mod jellyfin;
#[allow(dead_code)]
pub(crate) mod lastfm;
#[allow(dead_code)]
mod local;
mod panic_reporting;
mod platform_runtime;
#[allow(dead_code)]
mod plex;
mod radio;
mod remote_rating_wire;
mod removable;
#[allow(dead_code)]
mod server_playlist_coordinator;
#[allow(dead_code)]
mod source_lifecycle;
mod source_registry;
mod subsonic;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use adw::prelude::*;
use gtk::{gio, glib};
use tracing::{error, info};

/// Reverse-DNS application identifier.
const APP_ID: &str = "io.github.tributary.Tributary";

/// Route an application-level quit request through the active window so its
/// `close-request` handler can finish lifecycle-owned remote teardown first.
/// With no window there is no such barrier to join, so quitting the
/// application directly is the only remaining action.
fn dispatch_application_quit<W, CloseWindow, QuitApplication>(
    active_window: Option<W>,
    close_window: CloseWindow,
    quit_application: QuitApplication,
) where
    CloseWindow: FnOnce(W),
    QuitApplication: FnOnce(),
{
    if let Some(window) = active_window {
        close_window(window);
    } else {
        quit_application();
    }
}

fn request_application_quit(app: &adw::Application) {
    dispatch_application_quit(app.active_window(), |window| window.close(), || app.quit());
}

/// Prefer GTK's active window, but retain a structural fallback while a live
/// application window temporarily lacks active focus.
fn select_existing_window<W>(active: Option<W>, windows: impl IntoIterator<Item = W>) -> Option<W> {
    active.or_else(|| windows.into_iter().next())
}

const I18N_INITIALIZER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn initialize_i18n_backend() -> Result<(), String> {
    std::thread::Builder::new()
        .name("tributary-i18n-initializer".to_string())
        .stack_size(I18N_INITIALIZER_STACK_BYTES)
        .spawn(|| {
            // Force rust-i18n's generated LazyLock before a translation lookup
            // can initialize it on a platform's smaller main-thread stack.
            localization_catalog::initialize_backend();
        })
        .map_err(|error| format!("failed to start localization initializer: {error}"))?
        .join()
        .map_err(|_| "localization initializer stopped unexpectedly".to_string())
}

fn main() {
    // Rust invokes the global panic hook even for failures caught by a task
    // supervisor. Install a content-free hook before any application work so
    // panic payloads can never escape through the default formatter.
    panic_reporting::install_privacy_preserving_panic_hook();

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

    // Bundled toolkit paths and writable runtime caches must be configured
    // before GTK or GStreamer observes the process environment.
    match platform_runtime::configure_before_toolkit() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Tributary platform runtime setup failed: {error:#}");
            std::process::exit(1);
        }
    }

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

    // ── TLS crypto provider ──────────────────────────────────────────
    // rustls 0.23+ requires an explicit process-level CryptoProvider.
    // reqwest and sea-orm configure their own internally, but rust_cast
    // (Chromecast Cast V2) uses rustls directly on background threads.
    // Install the ring provider as the global default so all TLS
    // connections work without per-callsite configuration.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ── i18n: detect system locale ───────────────────────────────────
    if let Err(error) = initialize_i18n_backend() {
        error!(%error, "Could not initialize localization data");
        std::process::exit(1);
    }

    // Normalise the system locale for rust-i18n lookup:
    // - Unify underscore separators to hyphens ("zh_CN" → "zh-CN").
    // - Try the full locale first (e.g. "zh-CN", "pt-BR") to match
    //   region-specific translation files.
    // - Fall back to the base language code (e.g. "en-US" → "en") for
    //   languages that only have a single translation file.
    let raw_locale = sys_locale::get_locale().unwrap_or_else(|| "en".to_string());
    let normalised = raw_locale.replace('_', "-");
    let available = rust_i18n::available_locales!();
    let locale = if available.iter().any(|l| l == normalised.as_str()) {
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

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    // ── Application actions ─────────────────────────────────────────
    let quit_action = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| request_application_quit(app))
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

    // Claim the process-lifetime Last.fm playback coordinator before GTK can
    // deliver the first activation. The claim is never reusable, including
    // if the first window build drops its owner after a failure.
    let lastfm_playback_owner =
        match lastfm::playback_coordinator::LastFmPlaybackCoordinatorOwner::claim_process() {
            Ok(owner) => owner,
            Err(error) => {
                error!(category = %error, "Last.fm playback coordinator unavailable");
                std::process::exit(1);
            }
        };
    let lastfm_playback_owner = Rc::new(RefCell::new(Some(lastfm_playback_owner)));

    app.connect_activate(move |app| {
        // Single-instance guard: re-activating an already-running instance
        // (re-launching the binary, clicking the launcher/dock icon, or any
        // OS re-activation) re-emits `activate` on the primary instance.
        // Present the existing window instead of building a second one,
        // which would also register a duplicate OS media controller /
        // MPRIS service and double-fire media keys.
        if let Some(win) = select_existing_window(app.active_window(), app.windows()) {
            win.present();
            return;
        }

        // The process owner crosses into exactly the first window build. If
        // GTK activates again after that window disappeared or its build
        // failed, fail closed instead of constructing another coordinator.
        let Some(lastfm_playback_owner) = lastfm_playback_owner.borrow_mut().take() else {
            error!(
                category = "owner-consumed",
                "Last.fm playback coordinator unavailable during application activation"
            );
            return;
        };

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

        ui::window::build_window(
            app,
            rt_handle.clone(),
            engine_tx.clone(),
            engine_rx.clone(),
            lastfm_playback_owner,
        );
    });

    // ── File open handler (macOS "Open With" / Linux xdg-open) ────────
    //
    // When the OS opens files with Tributary (e.g. Finder → Open With),
    // GIO delivers them here.  We push them onto a thread-local queue
    // (see `ui::open_files`) and then either:
    //
    //   * activate the app — on first launch, the window is not yet
    //     built; the queue is drained at the end of `build_window`;
    //   * or, if a window is already live, fire the application-level
    //     `play-pending-files` GAction registered by `build_window` to
    //     drain the queue immediately.
    app.connect_open(move |app, files, _hint| {
        let mut paths = Vec::new();
        for file in files {
            if let Some(path) = file.path() {
                paths.push(path);
            }
        }
        if paths.is_empty() {
            return;
        }
        info!(count = paths.len(), "Files received via OS handler");
        ui::open_files::enqueue(paths);

        if app.active_window().is_none() {
            app.activate();
        } else if let Some(action) = app.lookup_action("play-pending-files") {
            action.activate(None);
        }
    });

    // Run the GTK main loop.  This blocks until the last window closes.
    let exit_code = app.run();
    std::process::exit(exit_code.into());
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::dispatch_application_quit;

    #[test]
    fn application_quit_closes_active_window_instead_of_bypassing_it() {
        let window_closed = Cell::new(false);
        let application_quit = Cell::new(false);

        dispatch_application_quit(
            Some(()),
            |()| window_closed.set(true),
            || application_quit.set(true),
        );

        assert!(window_closed.get());
        assert!(!application_quit.get());
    }

    #[test]
    fn application_quit_falls_back_when_no_window_exists() {
        let window_closed = Cell::new(false);
        let application_quit = Cell::new(false);

        dispatch_application_quit(
            None::<()>,
            |()| window_closed.set(true),
            || application_quit.set(true),
        );

        assert!(!window_closed.get());
        assert!(application_quit.get());
    }

    #[test]
    fn existing_window_selection_prefers_active_then_falls_back_structurally() {
        assert_eq!(super::select_existing_window(Some(7), [8, 9]), Some(7));
        assert_eq!(super::select_existing_window(None, [8, 9]), Some(8));
        assert_eq!(super::select_existing_window(None::<i32>, []), None);
    }
}
