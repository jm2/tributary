//! Tributary — A high-performance, Rhythmbox-style media manager.
//!
//! This is the application entry point. It initialises the tracing
//! subsystem, creates the GTK4/libadwaita application, and hands off
//! to the UI builder on activation.

#[allow(dead_code)]
mod architecture;
#[allow(dead_code)]
mod platform;
mod ui;

use adw::prelude::*;
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

    // ── GTK Application ──────────────────────────────────────────────
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(|app| {
        ui::window::build_window(app);
    });

    // Run the GTK main loop.  This blocks until the last window closes.
    let exit_code = app.run();
    std::process::exit(exit_code.into());
}
