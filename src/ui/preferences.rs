//! Preferences window — unified settings for browser views, column
//! visibility, and library location.
//!
//! Uses `adw::PreferencesDialog` with a single page containing three
//! groups: Browser Views, Visible Columns, and Library Location.

use adw::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::info;

// ── Default column visibility ───────────────────────────────────────────

/// All column titles in the tracklist, in display order.
pub const ALL_COLUMNS: &[&str] = &[
    "#",
    "Title",
    "Time",
    "Artist",
    "Album",
    "Genre",
    "Year",
    "Date Modified",
    "Bitrate",
    "Sample Rate",
    "Plays",
    "Format",
];

/// Columns visible by default.
const DEFAULT_VISIBLE: &[&str] = &["#", "Title", "Time", "Artist", "Album", "Genre"];

// ── Persisted configuration ─────────────────────────────────────────────

/// Application configuration persisted to `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Which browser panes are visible.
    pub browser_views: BrowserViewsConfig,
    /// Which tracklist columns are visible (by title).
    pub visible_columns: Vec<String>,
    /// Path to the local music library folder.
    pub library_path: String,
}

/// Browser pane visibility toggles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserViewsConfig {
    pub genre: bool,
    pub artist: bool,
    pub album: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        let music_dir = dirs::home_dir()
            .unwrap_or_default()
            .join("Music")
            .to_string_lossy()
            .to_string();

        Self {
            browser_views: BrowserViewsConfig {
                genre: true,
                artist: true,
                album: true,
            },
            visible_columns: DEFAULT_VISIBLE.iter().map(|s| s.to_string()).collect(),
            library_path: music_dir,
        }
    }
}

/// Path to the config file: `<data_dir>/tributary/config.json`
fn config_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("config.json"))
}

/// Load the configuration from disk, falling back to defaults.
pub fn load_config() -> AppConfig {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save the configuration to disk.
pub fn save_config(config: &AppConfig) {
    if let Some(path) = config_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(config) {
            let _ = std::fs::write(path, json);
        }
    }
}

// ── Preferences window builder ──────────────────────────────────────────

/// Build and present the preferences window.
///
/// # Arguments
/// * `parent` — the main application window (for transient-for)
/// * `column_view` — the tracklist `ColumnView` to toggle column visibility
/// * `browser_box` — the browser container `Box` to toggle pane visibility
/// * `config` — current configuration (will be mutated and saved on changes)
pub fn show_preferences(
    parent: &adw::ApplicationWindow,
    column_view: &gtk::ColumnView,
    browser_box: &gtk::Box,
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
) {
    let prefs_window = adw::PreferencesDialog::builder()
        .title("Preferences")
        .build();

    let page = adw::PreferencesPage::new();

    // ── Browser Views group ─────────────────────────────────────────
    let browser_group = adw::PreferencesGroup::builder()
        .title("Browser Views")
        .description("Toggle which filter panes are shown above the tracklist.")
        .build();

    let cfg = config.borrow();

    let genre_row = adw::SwitchRow::builder()
        .title("Genre")
        .active(cfg.browser_views.genre)
        .build();

    let artist_row = adw::SwitchRow::builder()
        .title("Artist")
        .active(cfg.browser_views.artist)
        .build();

    let album_row = adw::SwitchRow::builder()
        .title("Album")
        .active(cfg.browser_views.album)
        .build();

    browser_group.add(&genre_row);
    browser_group.add(&artist_row);
    browser_group.add(&album_row);

    // Wire browser view toggles
    {
        let config = config.clone();
        let browser_box = browser_box.clone();
        let artist_row_ref = artist_row.clone();
        let album_row_ref = album_row.clone();
        genre_row.connect_active_notify(move |row| {
            let mut cfg = config.borrow_mut();
            cfg.browser_views.genre = row.is_active();
            update_browser_visibility(&browser_box, &cfg.browser_views);
            save_config(&cfg);
            // Suppress unused variable warnings
            let _ = (&artist_row_ref, &album_row_ref);
        });
    }
    {
        let config = config.clone();
        let browser_box = browser_box.clone();
        let genre_row_ref = genre_row.clone();
        let album_row_ref = album_row.clone();
        artist_row.connect_active_notify(move |row| {
            let mut cfg = config.borrow_mut();
            cfg.browser_views.artist = row.is_active();
            update_browser_visibility(&browser_box, &cfg.browser_views);
            save_config(&cfg);
            let _ = (&genre_row_ref, &album_row_ref);
        });
    }
    {
        let config = config.clone();
        let browser_box = browser_box.clone();
        album_row.connect_active_notify(move |row| {
            let mut cfg = config.borrow_mut();
            cfg.browser_views.album = row.is_active();
            update_browser_visibility(&browser_box, &cfg.browser_views);
            save_config(&cfg);
        });
    }

    page.add(&browser_group);

    // ── Visible Columns group ───────────────────────────────────────
    let columns_group = adw::PreferencesGroup::builder()
        .title("Visible Columns")
        .description("Choose which columns appear in the tracklist.")
        .build();

    let column_switches: Vec<(&str, adw::SwitchRow)> = ALL_COLUMNS
        .iter()
        .map(|&col_title| {
            let is_visible = cfg.visible_columns.iter().any(|c| c == col_title);
            let row = adw::SwitchRow::builder()
                .title(col_title)
                .active(is_visible)
                .build();

            // Wire each column toggle
            let config = config.clone();
            let cv = column_view.clone();
            let title = col_title.to_string();
            row.connect_active_notify(move |row| {
                let mut cfg = config.borrow_mut();
                if row.is_active() {
                    if !cfg.visible_columns.contains(&title) {
                        cfg.visible_columns.push(title.clone());
                    }
                } else {
                    cfg.visible_columns.retain(|c| c != &title);
                }
                apply_column_visibility(&cv, &cfg.visible_columns);
                save_config(&cfg);
            });

            columns_group.add(&row);
            (col_title, row)
        })
        .collect();

    // Reset to Defaults button
    let reset_btn = gtk::Button::builder()
        .label("Reset to Defaults")
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .margin_top(8)
        .build();
    {
        let config = config.clone();
        let cv = column_view.clone();
        let switches = column_switches
            .iter()
            .map(|(t, r)| (t.to_string(), r.clone()))
            .collect::<Vec<_>>();
        reset_btn.connect_clicked(move |_| {
            let mut cfg = config.borrow_mut();
            cfg.visible_columns = DEFAULT_VISIBLE.iter().map(|s| s.to_string()).collect();
            for (title, row) in &switches {
                row.set_active(DEFAULT_VISIBLE.contains(&title.as_str()));
            }
            apply_column_visibility(&cv, &cfg.visible_columns);
            save_config(&cfg);
            info!("Column visibility reset to defaults");
        });
    }
    columns_group.add(&reset_btn);

    page.add(&columns_group);

    // ── Library Location group ──────────────────────────────────────
    let library_group = adw::PreferencesGroup::builder()
        .title("Library Location")
        .description("The folder Tributary scans for local music files.")
        .build();

    let library_row = adw::ActionRow::builder()
        .title("Music folder")
        .subtitle(&cfg.library_path)
        .build();

    let browse_btn = gtk::Button::builder()
        .label("Browse…")
        .valign(gtk::Align::Center)
        .build();
    library_row.add_suffix(&browse_btn);

    // Wire Browse button
    {
        let config = config.clone();
        let library_row = library_row.clone();
        let parent = parent.clone();
        browse_btn.connect_clicked(move |_| {
            let config = config.clone();
            let library_row = library_row.clone();
            let dialog = gtk::FileDialog::builder()
                .title("Select Music Folder")
                .modal(true)
                .build();

            dialog.select_folder(
                Some(&parent),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    if let Ok(folder) = result {
                        if let Some(path) = folder.path() {
                            let path_str = path.to_string_lossy().to_string();
                            info!(path = %path_str, "Library folder changed");
                            library_row.set_subtitle(&path_str);

                            let mut cfg = config.borrow_mut();
                            cfg.library_path = path_str;
                            save_config(&cfg);

                            // TODO: trigger DB wipe + re-scan via engine channel
                        }
                    }
                },
            );
        });
    }

    library_group.add(&library_row);
    page.add(&library_group);

    prefs_window.add(&page);
    drop(cfg); // release borrow before presenting

    prefs_window.present(Some(parent));
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Apply column visibility to the `ColumnView` based on the config.
pub fn apply_column_visibility(column_view: &gtk::ColumnView, visible: &[String]) {
    let columns = column_view.columns();
    for i in 0..columns.n_items() {
        if let Some(col) = columns.item(i).and_downcast_ref::<gtk::ColumnViewColumn>() {
            if let Some(title) = col.title() {
                col.set_visible(visible.iter().any(|v| v == title.as_str()));
            }
        }
    }
}

/// Update browser pane visibility based on config.
///
/// The browser `Box` contains three children (genre, artist, album
/// `ScrolledWindow` widgets).  If all three are hidden, hide the
/// entire box.
pub fn update_browser_visibility(browser_box: &gtk::Box, views: &BrowserViewsConfig) {
    // The browser box children are: genre_scrolled, artist_scrolled, album_scrolled
    let mut child_idx = 0;
    let mut child = browser_box.first_child();
    while let Some(widget) = child {
        let visible = match child_idx {
            0 => views.genre,
            1 => views.artist,
            2 => views.album,
            _ => true,
        };
        widget.set_visible(visible);
        child = widget.next_sibling();
        child_idx += 1;
    }

    // Hide the entire browser box if all panes are hidden.
    let any_visible = views.genre || views.artist || views.album;
    browser_box.set_visible(any_visible);
}
