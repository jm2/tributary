//! Preferences window — unified settings for library location, browser
//! views, and column visibility.
//!
//! Uses `adw::PreferencesDialog` with a single page containing three
//! groups: Library Location, Browser Views, and Visible Columns.

use adw::prelude::*;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{info, warn};

// ── Default column visibility ───────────────────────────────────────────

/// All column titles in the tracklist, in display order.
pub const ALL_COLUMNS: &[&str] = &[
    "#",
    "Title",
    "Time",
    "Artist",
    "Album",
    "Genre",
    "Composer",
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
    #[serde(default)]
    pub browser_views: BrowserViewsConfig,
    /// Which tracklist columns are visible (by title).
    #[serde(default = "default_visible_columns")]
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

/// Default visible columns (used for `#[serde(default)]`).
///
/// Having a serde default means an older/partial config that omits
/// `visible_columns` falls back field-by-field instead of discarding the
/// entire parsed config (which would also wipe the user's library paths).
fn default_visible_columns() -> Vec<String> {
    DEFAULT_VISIBLE
        .iter()
        .copied()
        .map(str::to_string)
        .collect()
}

/// Default library paths: the platform music directory (with a `~/Music`
/// fallback), but only if it actually exists on disk.
///
/// On a fresh profile with no music directory, this returns an empty list
/// rather than a phantom path — first launch then shows no library folders
/// instead of failing the initial scan/watch with a "folder not found" error.
/// This only affects the *default* (first launch, or a config missing the
/// field); paths a user has explicitly configured are preserved verbatim even
/// if they later go missing.
fn default_library_paths() -> Vec<String> {
    match dirs::audio_dir().or_else(|| dirs::home_dir().map(|h| h.join("Music"))) {
        Some(dir) if dir.is_dir() => vec![dir.to_string_lossy().to_string()],
        _ => Vec::new(),
    }
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

impl Default for BrowserViewsConfig {
    fn default() -> Self {
        // All three panes visible by default (matches `AppConfig::default`).
        Self {
            genre: true,
            artist: true,
            album: true,
        }
    }
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
/// Handles migration from the legacy `library_path` (single string)
/// format. We parse to a `serde_json::Value` first and rename the key
/// programmatically rather than doing a textual `String::replace`,
/// which would corrupt user-supplied values that happened to contain
/// the literal substring `"library_path"`.
pub fn load_config() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return AppConfig::default();
    };

    // Parse to a generic Value so we can rewrite the legacy key safely.
    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return AppConfig::default(),
    };

    if let Some(obj) = value.as_object_mut() {
        if !obj.contains_key("library_paths") {
            if let Some(legacy) = obj.remove("library_path") {
                obj.insert("library_paths".to_string(), legacy);
            }
        }
    }

    match serde_json::from_value(value) {
        Ok(config) => config,
        Err(e) => {
            warn!(error = %e, "Failed to deserialize config.json — falling back to defaults");
            AppConfig::default()
        }
    }
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
    on_album_artist_changed: std::rc::Rc<dyn Fn(bool)>,
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

    // The "+" (add) and the per-row "−" (remove) buttons all live inside this
    // one content box, so they share its trailing edge and line up by
    // construction — no DPI-fragile fixed margins. (A header-suffix "+" can't
    // be made to align with the rows, because adw lays the group header and
    // the row list out in separate containers with different insets.)
    let library_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    // "+" at the bottom-right of the group body, after the folder rows.
    let add_folder_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .halign(gtk::Align::End)
        .css_classes(["flat", "circular"])
        .tooltip_text("Add folder")
        .build();

    // Hint shown once the user adds or removes a folder. The running
    // library engine only reads the configured paths at startup, so a
    // restart is required before a newly-added folder is scanned/watched
    // (and a removed folder stops being watched). Hidden until a change.
    let restart_hint = gtk::Label::builder()
        .label("Restart Tributary to apply library folder changes")
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .visible(false)
        .margin_top(2)
        .build();

    // One row per folder: path on the left, "−" flush to the right edge.
    let paths_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    for lib_path in &cfg.library_paths {
        paths_box.append(&build_library_path_row(
            lib_path,
            config.clone(),
            paths_box.clone(),
            restart_hint.clone(),
        ));
    }
    library_box.append(&paths_box);
    library_box.append(&add_folder_btn);
    library_box.append(&restart_hint);

    // Add a folder via the file chooser.
    {
        let config = config.clone();
        let paths_box = paths_box.clone();
        let parent = parent.clone();
        let restart_hint = restart_hint.clone();
        add_folder_btn.connect_clicked(move |_| {
            let config = config.clone();
            let paths_box = paths_box.clone();
            let restart_hint = restart_hint.clone();
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
                                paths_box.clone(),
                                restart_hint.clone(),
                            );
                            paths_box.append(&row);
                            // The engine won't pick up the new folder until
                            // the next launch — tell the user a restart is
                            // needed instead of leaving them with an empty
                            // library.
                            restart_hint.set_visible(true);
                        }
                    }
                },
            );
        });
    }

    library_group.add(&library_box);
    page.add(&library_group);

    // ── Browser Views group (dense horizontal checkboxes) ───────────
    let browser_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.browser_views").as_ref())
        .build();

    // Same homogeneous 3-column grid as Visible Columns, so the two groups'
    // checkboxes line up column-to-column.
    let browser_grid = gtk::Grid::builder()
        .column_homogeneous(true)
        .row_spacing(4)
        .column_spacing(8)
        .hexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let genre_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("browser.genre").as_ref())
        .active(cfg.browser_views.genre)
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    let artist_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("browser.artist").as_ref())
        .active(cfg.browser_views.artist)
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    let album_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("browser.album").as_ref())
        .active(cfg.browser_views.album)
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();

    let album_artist_check = gtk::CheckButton::builder()
        .label("Group by Album Artist")
        .active(cfg.group_by_album_artist)
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();

    // Row 0: the three browser panes (one per grid column).
    browser_grid.attach(&genre_check, 0, 0, 1, 1);
    browser_grid.attach(&artist_check, 1, 0, 1, 1);
    browser_grid.attach(&album_check, 2, 0, 1, 1);
    // Row 1: the grouping toggle spans the full width (its label is longer).
    browser_grid.attach(&album_artist_check, 0, 1, 3, 1);

    // Wire album artist toggle
    {
        let config = config.clone();
        let on_change = on_album_artist_changed.clone();
        album_artist_check.connect_toggled(move |btn| {
            let active = btn.is_active();
            {
                let mut cfg = config.borrow_mut();
                cfg.group_by_album_artist = active;
                save_config(&cfg);
            }
            on_change(active);
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

    browser_group.add(&browser_grid);
    page.add(&browser_group);

    // ── Visible Columns group (dense grid with FlowBox) ─────────────
    let columns_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.visible_columns").as_ref())
        .build();

    // A homogeneous Grid (rather than a FlowBox) so every column is equal
    // width and the grid fills the group's clamped width: the leftmost column
    // is flush with the left edge and the rightmost with the right edge, and
    // the checkboxes line up column-to-column on every row.
    let columns_grid = gtk::Grid::builder()
        .column_homogeneous(true)
        .row_spacing(4)
        .column_spacing(8)
        .hexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    const COLUMNS_PER_ROW: usize = 4;

    let column_checks: Vec<(&str, gtk::CheckButton)> = ALL_COLUMNS
        .iter()
        .enumerate()
        .map(|(i, &col_title)| {
            let is_visible = cfg.visible_columns.iter().any(|c| c == col_title);
            let check = gtk::CheckButton::builder()
                .label(col_title)
                .active(is_visible)
                // Fill the homogeneous cell, but keep the label left-aligned
                // so column text aligns down each grid column.
                .hexpand(true)
                .halign(gtk::Align::Start)
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

            let col = (i % COLUMNS_PER_ROW) as i32;
            let row = (i / COLUMNS_PER_ROW) as i32;
            columns_grid.attach(&check, col, row, 1, 1);
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
            // Scope the mutable borrow so it is dropped before `set_active`
            // below. `set_active` synchronously re-enters each column's
            // `connect_toggled` handler, which takes its own `borrow_mut` —
            // holding the borrow across the loop would panic with
            // `BorrowMutError` (and abort across the GLib FFI boundary).
            {
                let mut cfg = config.borrow_mut();
                cfg.visible_columns = DEFAULT_VISIBLE
                    .iter()
                    .copied()
                    .map(str::to_string)
                    .collect();
                cfg.column_order = default_column_order();
            }
            for (title, check) in &checks {
                check.set_active(DEFAULT_VISIBLE.contains(&title.as_str()));
            }
            let cfg = config.borrow();
            apply_column_visibility(&cv, &cfg.visible_columns);
            apply_column_order(&cv, &cfg.column_order);
            save_config(&cfg);
            info!("Column visibility and order reset to defaults");
        });
    }

    columns_group.add(&columns_grid);
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

/// Build a library-folder row: the path (left, ellipsized) and its own "−"
/// remove button flush to the right edge, so it lines up under the group's
/// "+". A plain `Label` is used (no Pango markup), so no escaping is needed.
///
/// Removing a folder is always allowed — an empty list is a valid state (e.g.
/// first launch when no music folder exists yet).
fn build_library_path_row(
    path: &str,
    config: std::rc::Rc<std::cell::RefCell<AppConfig>>,
    paths_box: gtk::Box,
    restart_hint: gtk::Label,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();

    let label = gtk::Label::builder()
        .label(path)
        .hexpand(true)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .tooltip_text("Remove folder")
        .build();

    row.append(&label);
    row.append(&remove_btn);

    let path_owned = path.to_string();
    let row_clone = row.clone();
    remove_btn.connect_clicked(move |_| {
        {
            let mut cfg = config.borrow_mut();
            cfg.library_paths.retain(|p| p != &path_owned);
            save_config(&cfg);
        }
        paths_box.remove(&row_clone);
        // The engine keeps watching the removed folder until the next
        // launch — surface a restart hint so the stale tracks aren't
        // mistaken for a bug.
        restart_hint.set_visible(true);
    });

    row
}
