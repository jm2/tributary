//! Tributary — A high-performance, Rhythmbox-style media manager.
//!
//! This is the application entry point. It initialises the tracing
//! subsystem, spawns a tokio background runtime for async I/O,
//! creates the GTK4/libadwaita application, and hands off to the
//! UI builder on activation.

#[allow(dead_code)]
mod architecture;
mod audio;
mod db;
mod desktop_integration;
#[allow(dead_code)]
mod local;
#[allow(dead_code)]
mod platform;
mod ui;

use std::sync::Arc;
use std::thread;

use adw::prelude::*;
use gtk::gio;
use tracing::info;

/// Reverse-DNS application identifier.
const APP_ID: &str = "io.github.tributary.Tributary";

fn main() {
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
