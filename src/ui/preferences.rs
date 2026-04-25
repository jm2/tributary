//! Preferences window — unified settings for library location, browser
//! views, and column visibility.
//!
//! Uses `adw::PreferencesDialog` with a single page containing three
//! groups: Library Location, Browser Views, and Visible Columns.

use adw::prelude::*;
use serde::{Deserialize, Deserializer, Serialize};
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

/// Columns visible by default — all columns enabled.
const DEFAULT_VISIBLE: &[&str] = ALL_COLUMNS;

// ── Persisted configuration ─────────────────────────────────────────────

/// Application configuration persisted to `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Which browser panes are visible.
    pub browser_views: BrowserViewsConfig,
    /// Which tracklist columns are visible (by title).
    pub visible_columns: Vec<String>,
    /// Tracklist column display order (by title). Persisted across restarts.
    #[serde(default = "default_column_order")]
    pub column_order: Vec<String>,
    /// Paths to local music library folders.
    ///
    /// Migrated from the old single `library_path: String` field.
    /// The custom deserializer handles both formats seamlessly.
    #[serde(
        default = "default_library_paths",
        deserialize_with = "deserialize_library_paths"
    )]
    pub library_paths: Vec<String>,
    /// Whether the user has consented to IP-based geolocation for
    /// "Stations Near Me". `None` = not yet asked, `Some(true)` = accepted,
    /// `Some(false)` = declined.
    #[serde(default)]
    pub location_enabled: Option<bool>,
    /// Whether the browser Artist pane groups by Album Artist instead of
    /// the track-level Artist tag. Default: false (group by Artist).
    #[serde(default)]
    pub group_by_album_artist: bool,
}

/// Default column order (used for `#[serde(default)]`).
fn default_column_order() -> Vec<String> {
    ALL_COLUMNS.iter().copied().map(str::to_string).collect()
}

/// Default library paths: platform music directory with ~/Music fallback.
fn default_library_paths() -> Vec<String> {
    let music_dir = dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    vec![music_dir]
}

/// Custom deserializer that handles both the old `library_path: String`
/// format and the new `library_paths: Vec<String>` format seamlessly.
///
/// When reading an old config.json that has `"library_path": "/some/path"`,
/// serde will encounter this field name and type mismatch. We use an
/// untagged enum to try both representations.
fn deserialize_library_paths<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(s) => Ok(vec![s]),
        OneOrMany::Many(v) => Ok(v),
    }
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
        Self {
            browser_views: BrowserViewsConfig {
                genre: true,
                artist: true,
                album: true,
            },
            visible_columns: DEFAULT_VISIBLE
                .iter()
                .copied()
                .map(str::to_string)
                .collect(),
            column_order: default_column_order(),
            library_paths: default_library_paths(),
            location_enabled: None,
            group_by_album_artist: false,
        }
    }
}

impl AppConfig {
    /// Convenience getter: primary library path (first in the list).
    /// Used by callers that only need the main directory.
    #[allow(dead_code)] // Will be used by Chromecast and other features.
    pub fn primary_library_path(&self) -> &str {
        self.library_paths.first().map(|s| s.as_str()).unwrap_or("")
    }
}

/// Path to the config file: `<data_dir>/tributary/config.json`
fn config_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("config.json"))
}

/// Load the configuration from disk, falling back to defaults.
///
/// Handles migration from the old `library_path` (single string) format:
/// the raw JSON is pre-processed to rename the key before deserialization.
pub fn load_config() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return AppConfig::default();
    };

    // Pre-process: if the JSON has "library_path" but not "library_paths",
    // rename the key so serde's custom deserializer receives it correctly.
    let json_str = if raw.contains("\"library_path\"") && !raw.contains("\"library_paths\"") {
        raw.replace("\"library_path\"", "\"library_paths\"")
    } else {
        raw
    };

    serde_json::from_str(&json_str).unwrap_or_default()
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
    let prefs_dialog = adw::PreferencesDialog::builder()
        .title(rust_i18n::t!("preferences.title").as_ref())
        .build();

    let page = adw::PreferencesPage::new();
    let cfg = config.borrow();

    // ── Library Location group (supports multiple folders) ──────────
    let library_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.library_location").as_ref())
        .build();

    // Container for per-path rows (lives inside the group).
    let paths_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();

    let paths_box_rc = std::rc::Rc::new(std::cell::RefCell::new(paths_box.clone()));

    // Build a row for each existing library path.
    for lib_path in &cfg.library_paths {
        let row = build_library_path_row(lib_path, config.clone(), paths_box_rc.clone());
        paths_box.append(&row);
    }

    // "Add Folder…" button.
    let add_folder_btn = gtk::Button::builder()
        .label(rust_i18n::t!("preferences.browse").as_ref())
        .icon_name("list-add-symbolic")
        .halign(gtk::Align::Start)
        .css_classes(["flat"])
        .build();
    {
        let config = config.clone();
        let paths_box_rc = paths_box_rc.clone();
        let parent = parent.clone();
        add_folder_btn.connect_clicked(move |_| {
            let config = config.clone();
            let paths_box_rc = paths_box_rc.clone();
            let dialog = gtk::FileDialog::builder()
                .title(rust_i18n::t!("preferences.select_music_folder").as_ref())
                .modal(true)
                .build();

            dialog.select_folder(
                Some(&parent),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    if let Ok(folder) = result {
                        if let Some(path) = folder.path() {
                            let path_str = path.to_string_lossy().to_string();
                            // Don't add duplicates.
                            let mut cfg = config.borrow_mut();
                            if cfg.library_paths.contains(&path_str) {
                                return;
                            }
                            info!(path = %path_str, "Library folder added");
                            cfg.library_paths.push(path_str.clone());
                            save_config(&cfg);
                            drop(cfg);

                            let row = build_library_path_row(
                                &path_str,
                                config.clone(),
                                paths_box_rc.clone(),
                            );
                            paths_box_rc.borrow().append(&row);
                        }
                    }
                },
            );
        });
    }

    library_group.add(&paths_box);
    library_group.add(&add_folder_btn);
    page.add(&library_group);

    // ── Browser Views group (dense horizontal checkboxes) ───────────
    let browser_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.browser_views").as_ref())
        .build();

    let browser_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let genre_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("browser.genre").as_ref())
        .active(cfg.browser_views.genre)
        .build();
    let artist_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("browser.artist").as_ref())
        .active(cfg.browser_views.artist)
        .build();
    let album_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("browser.album").as_ref())
        .active(cfg.browser_views.album)
        .build();

    let album_artist_check = gtk::CheckButton::builder()
        .label("Group by Album Artist")
        .active(cfg.group_by_album_artist)
        .build();

    browser_row.append(&genre_check);
    browser_row.append(&artist_check);
    browser_row.append(&album_check);
    browser_row.append(&album_artist_check);

    // Wire album artist toggle
    {
        let config = config.clone();
        album_artist_check.connect_toggled(move |btn| {
            let mut cfg = config.borrow_mut();
            cfg.group_by_album_artist = btn.is_active();
            save_config(&cfg);
        });
    }

    // Wire browser view toggles
    {
        let config = config.clone();
        let browser_box = browser_box.clone();
        genre_check.connect_toggled(move |btn| {
            let mut cfg = config.borrow_mut();
            cfg.browser_views.genre = btn.is_active();
            update_browser_visibility(&browser_box, &cfg.browser_views);
            save_config(&cfg);
        });
    }
    {
        let config = config.clone();
        let browser_box = browser_box.clone();
        artist_check.connect_toggled(move |btn| {
            let mut cfg = config.borrow_mut();
            cfg.browser_views.artist = btn.is_active();
            update_browser_visibility(&browser_box, &cfg.browser_views);
            save_config(&cfg);
        });
    }
    {
        let config = config.clone();
        let browser_box = browser_box.clone();
        album_check.connect_toggled(move |btn| {
            let mut cfg = config.borrow_mut();
            cfg.browser_views.album = btn.is_active();
            update_browser_visibility(&browser_box, &cfg.browser_views);
            save_config(&cfg);
        });
    }

    browser_group.add(&browser_row);
    page.add(&browser_group);

    // ── Visible Columns group (dense grid with FlowBox) ─────────────
    let columns_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.visible_columns").as_ref())
        .build();

    let flow_box = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(3)
        .min_children_per_line(3)
        .homogeneous(true)
        .row_spacing(4)
        .column_spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let column_checks: Vec<(&str, gtk::CheckButton)> = ALL_COLUMNS
        .iter()
        .map(|&col_title| {
            let is_visible = cfg.visible_columns.iter().any(|c| c == col_title);
            let check = gtk::CheckButton::builder()
                .label(col_title)
                .active(is_visible)
                .build();

            // Wire each column toggle
            let config = config.clone();
            let cv = column_view.clone();
            let title = col_title.to_string();
            check.connect_toggled(move |btn| {
                let mut cfg = config.borrow_mut();
                if btn.is_active() {
                    if !cfg.visible_columns.contains(&title) {
                        cfg.visible_columns.push(title.clone());
                    }
                } else {
                    cfg.visible_columns.retain(|c| c != &title);
                }
                apply_column_visibility(&cv, &cfg.visible_columns);
                save_config(&cfg);
            });

            flow_box.append(&check);
            (col_title, check)
        })
        .collect();

    // Reset to Defaults button
    let reset_btn = gtk::Button::builder()
        .label(rust_i18n::t!("preferences.reset_to_defaults").as_ref())
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .margin_top(4)
        .build();
    {
        let config = config.clone();
        let cv = column_view.clone();
        let checks = column_checks
            .iter()
            .map(|(t, c)| ((*t).to_string(), c.clone()))
            .collect::<Vec<_>>();
        reset_btn.connect_clicked(move |_| {
            let mut cfg = config.borrow_mut();
            cfg.visible_columns = DEFAULT_VISIBLE
                .iter()
                .copied()
                .map(str::to_string)
                .collect();
            cfg.column_order = default_column_order();
            for (title, check) in &checks {
                check.set_active(DEFAULT_VISIBLE.contains(&title.as_str()));
            }
            apply_column_visibility(&cv, &cfg.visible_columns);
            apply_column_order(&cv, &cfg.column_order);
            save_config(&cfg);
            info!("Column visibility and order reset to defaults");
        });
    }

    columns_group.add(&flow_box);
    columns_group.add(&reset_btn);
    page.add(&columns_group);

    prefs_dialog.add(&page);
    drop(cfg);

    prefs_dialog.present(Some(parent));
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Apply column visibility to the `ColumnView` based on the config.
///
/// Skips the sentinel column (empty title) used to absorb GTK4's
/// rightmost-column auto-expansion.
pub fn apply_column_visibility(column_view: &gtk::ColumnView, visible: &[String]) {
    let columns = column_view.columns();
    for i in 0..columns.n_items() {
        if let Some(col) = columns.item(i).and_downcast_ref::<gtk::ColumnViewColumn>() {
            if let Some(title) = col.title() {
                if title.is_empty() {
                    continue; // sentinel column
                }
                col.set_visible(visible.iter().any(|v| v == title.as_str()));
            }
        }
    }
}

/// Apply persisted column order to the `ColumnView`.
///
/// Iterates the saved order and moves each column to its target position
/// using `insert_column` (which also removes from the old position).
pub fn apply_column_order(column_view: &gtk::ColumnView, order: &[String]) {
    if order.is_empty() {
        return;
    }
    for (target_pos, title) in order.iter().enumerate() {
        let columns = column_view.columns();
        // Find the column with this title at its current position.
        let mut found_at = None;
        for i in 0..columns.n_items() {
            if let Some(col) = columns.item(i).and_downcast_ref::<gtk::ColumnViewColumn>() {
                if let Some(col_title) = col.title() {
                    if col_title.as_str() == title {
                        found_at = Some((i, col.clone()));
                        break;
                    }
                }
            }
        }
        if let Some((current_pos, col)) = found_at {
            if current_pos as usize != target_pos {
                column_view.remove_column(&col);
                column_view.insert_column(target_pos as u32, &col);
            }
        }
    }
}

/// Read the current column order from the `ColumnView`.
///
/// Skips the sentinel column (empty title).
pub fn read_column_order(column_view: &gtk::ColumnView) -> Vec<String> {
    let columns = column_view.columns();
    let mut order = Vec::new();
    for i in 0..columns.n_items() {
        if let Some(col) = columns.item(i).and_downcast_ref::<gtk::ColumnViewColumn>() {
            if let Some(title) = col.title() {
                if !title.is_empty() {
                    order.push(title.to_string());
                }
            }
        }
    }
    order
}

/// Update browser pane visibility based on config.
///
/// The browser `Box` now has a vertical layout: SearchEntry at the top,
/// then a horizontal panes_box containing three children (genre, artist,
/// album pane `Box` widgets).  If all three panes are hidden, hide the
/// entire browser box.  The search entry visibility follows the browser.
pub fn update_browser_visibility(browser_box: &gtk::Box, views: &BrowserViewsConfig) {
    // The browser_box layout is: SearchEntry, panes_box (horizontal Box).
    // Find the panes_box (last child, which is a horizontal Box).
    let panes_box = browser_box
        .last_child()
        .and_then(|w| w.downcast::<gtk::Box>().ok());

    if let Some(ref panes_box) = panes_box {
        let mut child_idx = 0;
        let mut child = panes_box.first_child();
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
    }

    let any_visible = views.genre || views.artist || views.album;
    browser_box.set_visible(any_visible);
}

/// Build a single library-path row for the preferences dialog.
///
/// Each row shows the folder path as an `ActionRow` with a "Remove" button.
/// Removing the last path is prevented (at least one directory is required).
fn build_library_path_row(
    path: &str,
    config: std::rc::Rc<std::cell::RefCell<AppConfig>>,
    paths_box: std::rc::Rc<std::cell::RefCell<gtk::Box>>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(rust_i18n::t!("preferences.music_folder").as_ref())
        .subtitle(path)
        .build();

    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .tooltip_text("Remove folder")
        .build();
    row.add_suffix(&remove_btn);

    let path_owned = path.to_string();
    let row_clone = row.clone();
    remove_btn.connect_clicked(move |_| {
        let mut cfg = config.borrow_mut();
        // Prevent removing the last directory.
        if cfg.library_paths.len() <= 1 {
            return;
        }
        cfg.library_paths.retain(|p| p != &path_owned);
        save_config(&cfg);
        drop(cfg);
        paths_box.borrow().remove(&row_clone);
    });

    row
}
