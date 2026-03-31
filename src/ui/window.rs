//! Main application window.
//!
//! Constructs a modern libadwaita window with a header bar and a
//! placeholder welcome page.  This will evolve into the full
//! Rhythmbox-style multi-pane interface in later phases.

use adw::prelude::*;
use gtk::Align;
use tracing::info;

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 1200;
/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 800;

/// Build and present the main Tributary window.
pub fn build_window(app: &adw::Application) {
    info!("Building main window");

    // ── Header Bar ───────────────────────────────────────────────────
    let header = adw::HeaderBar::builder()
        .title_widget(
            &adw::WindowTitle::builder()
                .title("Tributary")
                .subtitle("Music Manager")
                .build(),
        )
        .build();

    // ── Welcome Content ──────────────────────────────────────────────
    let status_page = adw::StatusPage::builder()
        .icon_name("folder-music-symbolic")
        .title("Welcome to Tributary")
        .description("Your music library is empty.\nAdd a music folder to get started.")
        .vexpand(true)
        .valign(Align::Center)
        .build();

    // ── Main Layout ──────────────────────────────────────────────────
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&status_page);

    // ── Window ───────────────────────────────────────────────────────
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Tributary")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&content)
        .build();

    // Respect the system colour scheme (light / dark).
    let style_manager = adw::StyleManager::default();
    info!(
        "System colour scheme: {:?}",
        style_manager.color_scheme()
    );

    window.present();
    info!("Main window presented");
}
