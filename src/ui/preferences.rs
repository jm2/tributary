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
    "Rating",
    "Format",
];

/// Columns visible by default — all columns enabled.
const DEFAULT_VISIBLE: &[&str] = ALL_COLUMNS;
const CURRENT_COLUMN_SCHEMA_VERSION: u32 = 1;

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
    /// One-time evolution marker for newly introduced tracklist columns.
    /// Zero identifies configurations written before this field existed.
    #[serde(default)]
    pub column_schema_version: u32,
    /// Paths to local music library folders.
    ///
    /// Migrated from the old single `library_path: String` field.
    /// The custom deserializer handles both formats seamlessly.
    #[serde(
        default = "default_library_paths",
        deserialize_with = "deserialize_library_paths"
    )]
    pub library_paths: Vec<String>,
    /// Explicit requests to reauthorize an existing library through a new
    /// portal path while preserving that library's identity.
    ///
    /// `library_paths` deliberately continues to contain `old_path` until the
    /// engine has applied the guarded relocation. This prevents a restart
    /// between selection and migration from indexing the same library as a
    /// second, unrelated root.
    #[serde(default)]
    pub pending_root_reauthorizations: Vec<PendingRootReauthorization>,
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

/// A user-confirmed old-to-new library-root reauthorization.
///
/// The UUID-shaped request ID lets the engine make applying the request
/// idempotent. A pending request is immutable until the engine commits or
/// rejects it, preventing an in-flight result from being applied to a newer
/// destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRootReauthorization {
    pub request_id: String,
    pub old_path: String,
    pub new_path: String,
}

/// Result of scheduling a root reauthorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootReauthorizationSchedule {
    Scheduled { request_id: String },
}

/// Validation failure for a root reauthorization request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootReauthorizationError {
    SourceMissing,
    SamePath,
    OverlappingPath,
    DuplicateDestination,
    PendingRequest,
    UnsupportedPathEncoding,
    InvalidRequestId,
    ConfigSaveFailed,
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
            column_schema_version: CURRENT_COLUMN_SCHEMA_VERSION,
            library_paths: default_library_paths(),
            pending_root_reauthorizations: Vec::new(),
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

/// Validate a proposed identity-preserving library-root reauthorization.
///
/// Path comparisons are deliberately exact: these are the persisted paths
/// used for root identity, and resolving them against the host filesystem is
/// neither reliable nor necessarily permitted inside a sandbox.
pub fn validate_root_reauthorization(
    config: &AppConfig,
    old_path: &str,
    new_path: &str,
) -> Result<(), RootReauthorizationError> {
    if !config.library_paths.iter().any(|path| path == old_path) {
        return Err(RootReauthorizationError::SourceMissing);
    }
    if old_path == new_path {
        return Err(RootReauthorizationError::SamePath);
    }
    if library_paths_overlap(old_path, new_path) {
        return Err(RootReauthorizationError::OverlappingPath);
    }
    if config
        .library_paths
        .iter()
        .any(|path| path != old_path && library_paths_overlap(path, new_path))
        || config.pending_root_reauthorizations.iter().any(|pending| {
            library_paths_overlap(&pending.old_path, new_path)
                || library_paths_overlap(&pending.new_path, new_path)
        })
    {
        return Err(RootReauthorizationError::DuplicateDestination);
    }
    Ok(())
}

/// Component-aware scope overlap without filesystem access or canonicalizing
/// a path the sandbox may not be authorized to inspect.
pub fn library_paths_overlap(left: &str, right: &str) -> bool {
    let left = std::path::Path::new(left);
    let right = std::path::Path::new(right);
    left.starts_with(right) || right.starts_with(left)
}

/// Whether a path is already configured or reserved as the destination of a
/// pending identity-preserving move.
pub fn library_path_is_claimed(config: &AppConfig, path: &str) -> bool {
    config
        .library_paths
        .iter()
        // Nested ordinary roots are an existing supported configuration;
        // only an exact duplicate is rejected when no identity move owns the
        // scope. Pending endpoints are stricter because overlap could race a
        // relocation and mint duplicate track identities.
        .any(|existing| existing == path)
        || config.pending_root_reauthorizations.iter().any(|pending| {
            library_paths_overlap(&pending.old_path, path)
                || library_paths_overlap(&pending.new_path, path)
        })
}

/// Schedule an identity-preserving root reauthorization.
///
/// `request_id` must be a UUID string. A root with a pending request is locked
/// until the engine commits or cleanly rejects that exact request; silently
/// superseding it could let an in-flight result update the wrong destination.
pub fn schedule_root_reauthorization(
    config: &mut AppConfig,
    old_path: &str,
    new_path: &str,
    request_id: &str,
) -> Result<RootReauthorizationSchedule, RootReauthorizationError> {
    if config
        .pending_root_reauthorizations
        .iter()
        .any(|pending| pending.old_path == old_path)
    {
        return Err(RootReauthorizationError::PendingRequest);
    }
    validate_root_reauthorization(config, old_path, new_path)?;

    if uuid::Uuid::parse_str(request_id).is_err()
        || config
            .pending_root_reauthorizations
            .iter()
            .any(|pending| pending.request_id == request_id)
    {
        return Err(RootReauthorizationError::InvalidRequestId);
    }
    config
        .pending_root_reauthorizations
        .push(PendingRootReauthorization {
            request_id: request_id.to_string(),
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        });
    Ok(RootReauthorizationSchedule::Scheduled {
        request_id: request_id.to_string(),
    })
}

/// Remove a configured root unless an in-flight reauthorization owns it.
///
/// A pending request must first be committed or exact-CAS rejected; otherwise
/// removing its source could race the engine result and erase identity intent.
pub fn remove_library_path(config: &mut AppConfig, path: &str) -> bool {
    if config
        .pending_root_reauthorizations
        .iter()
        .any(|pending| pending.old_path == path)
    {
        return false;
    }
    let original_len = config.library_paths.len();
    config.library_paths.retain(|configured| configured != path);
    config.library_paths.len() != original_len
}

/// Apply a completed reauthorization only when config still contains the
/// exact request the engine processed.
///
/// This compare-and-swap prevents a late completion event from overwriting a
/// destination the user superseded in the meantime. Malformed duplicate
/// source, destination, or request-ID claims fail without mutating config.
pub fn complete_root_reauthorization(
    config: &mut AppConfig,
    request_id: &str,
    old_path: &str,
    new_path: &str,
) -> bool {
    let matching_intents = config
        .pending_root_reauthorizations
        .iter()
        .enumerate()
        .filter(|(_, pending)| {
            pending.request_id == request_id
                && pending.old_path == old_path
                && pending.new_path == new_path
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matching_intents.len() != 1
        || config
            .pending_root_reauthorizations
            .iter()
            .filter(|pending| pending.request_id == request_id || pending.old_path == old_path)
            .count()
            != 1
        || config
            .pending_root_reauthorizations
            .iter()
            .any(|pending| pending.new_path == new_path && pending.request_id != request_id)
    {
        return false;
    }

    let old_positions = config
        .library_paths
        .iter()
        .enumerate()
        .filter(|(_, path)| path.as_str() == old_path)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if old_positions.len() != 1
        || library_paths_overlap(old_path, new_path)
        || config
            .library_paths
            .iter()
            .enumerate()
            .any(|(index, path)| {
                index != old_positions[0]
                    && (library_paths_overlap(path, old_path)
                        || library_paths_overlap(path, new_path))
            })
    {
        return false;
    }

    config.library_paths[old_positions[0]] = new_path.to_string();
    config
        .pending_root_reauthorizations
        .remove(matching_intents[0]);
    true
}

/// Remove a cleanly rejected request only when it still exactly matches the
/// engine result. The configured library roots are intentionally unchanged.
pub fn reject_root_reauthorization(
    config: &mut AppConfig,
    request_id: &str,
    old_path: &str,
    new_path: &str,
) -> bool {
    let matches = config
        .pending_root_reauthorizations
        .iter()
        .enumerate()
        .filter(|(_, pending)| {
            pending.request_id == request_id
                && pending.old_path == old_path
                && pending.new_path == new_path
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1
        || config
            .pending_root_reauthorizations
            .iter()
            .filter(|pending| pending.request_id == request_id || pending.old_path == old_path)
            .count()
            != 1
    {
        return false;
    }
    let old_positions = config
        .library_paths
        .iter()
        .enumerate()
        .filter(|(_, path)| path.as_str() == old_path)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if old_positions.len() != 1
        || library_paths_overlap(old_path, new_path)
        || config
            .library_paths
            .iter()
            .enumerate()
            .any(|(index, path)| {
                index != old_positions[0]
                    && (library_paths_overlap(path, old_path)
                        || library_paths_overlap(path, new_path))
            })
    {
        return false;
    }

    config.pending_root_reauthorizations.remove(matches[0]);
    true
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
        Ok(mut config) => {
            migrate_column_schema(&mut config);
            config
        }
        Err(e) => {
            warn!(error = %e, "Failed to deserialize config.json — falling back to defaults");
            AppConfig::default()
        }
    }
}

/// Expose each newly introduced column exactly once for established profiles.
///
/// Column titles are persistence keys. A version marker distinguishes an old
/// profile that could not mention Rating from a current profile where the user
/// intentionally hid or reordered it.
fn migrate_column_schema(config: &mut AppConfig) {
    if config.column_schema_version >= CURRENT_COLUMN_SCHEMA_VERSION {
        return;
    }

    if !config.column_order.iter().any(|title| title == "Rating") {
        let insertion = config
            .column_order
            .iter()
            .position(|title| title == "Plays")
            .map_or(config.column_order.len(), |index| index + 1);
        config.column_order.insert(insertion, "Rating".to_string());
    }
    if !config.visible_columns.iter().any(|title| title == "Rating") {
        config.visible_columns.push("Rating".to_string());
    }
    config.column_schema_version = CURRENT_COLUMN_SCHEMA_VERSION;
}

/// Save the configuration through an atomic same-directory replacement.
///
/// A failed serialization, write, flush, or rename leaves the previous
/// `config.json` untouched. The boolean lets actions whose correctness
/// depends on persistence avoid claiming success, while legacy callers may
/// continue relying on the error log for best-effort preference changes.
pub fn save_config(config: &AppConfig) -> bool {
    use std::io::Write;

    let Some(path) = config_path() else {
        warn!("Cannot save config.json because the platform data directory is unavailable");
        return false;
    };
    let Some(parent) = path.parent() else {
        warn!(path = %path.display(), "Cannot save config.json without a parent directory");
        return false;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        warn!(error = %error, path = %parent.display(), "Failed to create config directory");
        return false;
    }
    let json = match serde_json::to_vec_pretty(config) {
        Ok(json) => json,
        Err(error) => {
            warn!(error = %error, "Failed to serialize config.json");
            return false;
        }
    };
    let mut temporary = match tempfile::NamedTempFile::new_in(parent) {
        Ok(temporary) => temporary,
        Err(error) => {
            warn!(error = %error, path = %parent.display(), "Failed to create temporary config file");
            return false;
        }
    };
    if let Err(error) = temporary
        .write_all(&json)
        .and_then(|()| temporary.write_all(b"\n"))
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
    {
        warn!(error = %error, "Failed to durably write temporary config file");
        return false;
    }
    if let Err(error) = temporary.persist(&path) {
        warn!(error = %error, path = %path.display(), "Failed to atomically replace config.json");
        return false;
    }

    // On Unix, syncing the containing directory makes the rename durable
    // across a sudden power loss. The file itself was synchronized above.
    #[cfg(unix)]
    match std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => {}
        Err(error) => {
            // The atomic replacement has already succeeded, so returning
            // failure here would incorrectly prompt the caller to retry an
            // update that is visible to this process.
            warn!(error = %error, path = %parent.display(), "Could not synchronize config directory metadata");
        }
    }

    true
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
    let restart_hint_text = if cfg.pending_root_reauthorizations.is_empty() {
        rust_i18n::t!("preferences.library_restart_hint")
    } else {
        rust_i18n::t!("preferences.reauthorization_restart_hint")
    };
    let restart_hint = gtk::Label::builder()
        .label(restart_hint_text.as_ref())
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .visible(!cfg.pending_root_reauthorizations.is_empty())
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
            parent.clone(),
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
            let parent_for_result = parent.clone();

            dialog.select_folder(
                Some(&parent),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    if let Ok(folder) = result {
                        if let Some(path) = folder.path() {
                            let Some(path_str) = path.to_str().map(str::to_string) else {
                                warn!("Ignoring a selected library folder with a non-Unicode path");
                                return;
                            };
                            // Do not add duplicate/overlapping configured or
                            // pending scopes. Persist the candidate before
                            // publishing it to the running UI.
                            let added = {
                                let mut cfg = config.borrow_mut();
                                if library_path_is_claimed(&cfg, &path_str) {
                                    false
                                } else {
                                    let mut candidate = cfg.clone();
                                    candidate.library_paths.push(path_str.clone());
                                    if save_config(&candidate) {
                                        *cfg = candidate;
                                        true
                                    } else {
                                        false
                                    }
                                }
                            };
                            if !added {
                                return;
                            }
                            info!(path = %path_str, "Library folder added");

                            let row = build_library_path_row(
                                &path_str,
                                config.clone(),
                                paths_box.clone(),
                                restart_hint.clone(),
                                parent_for_result.clone(),
                            );
                            paths_box.append(&row);
                            // The engine won't pick up the new folder until
                            // the next launch — tell the user a restart is
                            // needed instead of leaving them with an empty
                            // library.
                            restart_hint.set_label(
                                rust_i18n::t!("preferences.library_restart_hint").as_ref(),
                            );
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
/// An empty list is valid (for example on first launch), but a root with an
/// in-flight reauthorization is locked until its exact intent settles.
fn build_library_path_row(
    path: &str,
    config: std::rc::Rc<std::cell::RefCell<AppConfig>>,
    paths_box: gtk::Box,
    restart_hint: gtk::Label,
    parent: adw::ApplicationWindow,
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

    let reauthorize_btn = gtk::Button::builder()
        .label(rust_i18n::t!("preferences.reauthorize_folder").as_ref())
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();

    let has_pending_request = config
        .borrow()
        .pending_root_reauthorizations
        .iter()
        .any(|pending| pending.old_path == path);
    reauthorize_btn.set_sensitive(!has_pending_request);
    remove_btn.set_sensitive(!has_pending_request);

    row.append(&label);
    row.append(&reauthorize_btn);
    row.append(&remove_btn);

    let old_path = path.to_string();
    {
        let config = config.clone();
        let parent = parent.clone();
        let restart_hint = restart_hint.clone();
        let reauthorize_btn_for_state = reauthorize_btn.clone();
        let remove_btn_for_state = remove_btn.clone();
        reauthorize_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title(
                    rust_i18n::t!("preferences.select_reauthorization_folder").as_ref(),
                )
                .modal(true)
                .build();
            let config = config.clone();
            let parent = parent.clone();
            let old_path = old_path.clone();
            let restart_hint = restart_hint.clone();
            let reauthorize_btn = reauthorize_btn_for_state.clone();
            let remove_btn = remove_btn_for_state.clone();
            let parent_for_result = parent.clone();
            dialog.select_folder(
                Some(&parent),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    let Ok(folder) = result else {
                        // Closing the chooser is not an error and needs no
                        // additional prompt.
                        return;
                    };
                    let Some(path) = folder.path() else {
                        present_reauthorization_error(
                            &parent_for_result,
                            None,
                            &old_path,
                            None,
                        );
                        return;
                    };
                    let Some(new_path) = path.to_str().map(str::to_string) else {
                        present_reauthorization_error(
                            &parent_for_result,
                            Some(RootReauthorizationError::UnsupportedPathEncoding),
                            &old_path,
                            None,
                        );
                        return;
                    };
                    if let Err(error) = validate_root_reauthorization(
                        &config.borrow(),
                        &old_path,
                        &new_path,
                    ) {
                        present_reauthorization_error(
                            &parent_for_result,
                            Some(error),
                            &old_path,
                            Some(&new_path),
                        );
                        return;
                    }

                    let body = rust_i18n::t!(
                        "preferences.reauthorization_confirmation_body",
                        old_path = old_path.clone(),
                        new_path = new_path.clone()
                    );
                    let confirmation = adw::AlertDialog::builder()
                        .heading(
                            rust_i18n::t!("preferences.reauthorization_confirmation_heading")
                                .as_ref(),
                        )
                        .body(body.as_ref())
                        .close_response("cancel")
                        .default_response("cancel")
                        .build();
                    confirmation.add_response(
                        "cancel",
                        rust_i18n::t!("dialogs.cancel").as_ref(),
                    );
                    confirmation.add_response(
                        "reauthorize",
                        rust_i18n::t!("preferences.confirm_reauthorization").as_ref(),
                    );
                    confirmation.set_response_appearance(
                        "reauthorize",
                        adw::ResponseAppearance::Suggested,
                    );

                    let config = config.clone();
                    let parent = parent_for_result.clone();
                    let parent_for_response = parent.clone();
                    let restart_hint = restart_hint.clone();
                    let reauthorize_btn = reauthorize_btn.clone();
                    let remove_btn = remove_btn.clone();
                    confirmation.connect_response(None, move |_dialog, response| {
                        if response != "reauthorize" {
                            return;
                        }

                        let request_id = uuid::Uuid::new_v4().to_string();
                        let result = {
                            let mut cfg = config.borrow_mut();
                            let mut candidate = cfg.clone();
                            let result = schedule_root_reauthorization(
                                &mut candidate,
                                &old_path,
                                &new_path,
                                &request_id,
                            );
                            if result.is_ok() && save_config(&candidate) {
                                *cfg = candidate;
                                result
                            } else if result.is_ok() {
                                Err(RootReauthorizationError::ConfigSaveFailed)
                            } else {
                                result
                            }
                        };
                        match result {
                            Ok(RootReauthorizationSchedule::Scheduled { request_id }) => {
                                info!(%request_id, old_path = %old_path, new_path = %new_path, "Library root reauthorization scheduled");
                                restart_hint.set_label(
                                    rust_i18n::t!(
                                        "preferences.reauthorization_restart_hint"
                                    )
                                    .as_ref(),
                                );
                                restart_hint.set_visible(true);
                                reauthorize_btn.set_sensitive(false);
                                remove_btn.set_sensitive(false);
                            }
                            Err(error) => present_reauthorization_error(
                                &parent_for_response,
                                Some(error),
                                &old_path,
                                Some(&new_path),
                            ),
                        }
                    });
                    confirmation.present(Some(&parent));
                },
            );
        });
    }

    let path_owned = path.to_string();
    let row_clone = row.clone();
    remove_btn.connect_clicked(move |_| {
        let removed = {
            let mut cfg = config.borrow_mut();
            let mut candidate = cfg.clone();
            if remove_library_path(&mut candidate, &path_owned) && save_config(&candidate) {
                *cfg = candidate;
                true
            } else {
                false
            }
        };
        if !removed {
            return;
        }
        paths_box.remove(&row_clone);
        // The engine keeps watching the removed folder until the next
        // launch — surface a restart hint so the stale tracks aren't
        // mistaken for a bug.
        restart_hint.set_label(rust_i18n::t!("preferences.library_restart_hint").as_ref());
        restart_hint.set_visible(true);
    });

    row
}

fn present_reauthorization_error(
    parent: &adw::ApplicationWindow,
    error: Option<RootReauthorizationError>,
    old_path: &str,
    new_path: Option<&str>,
) {
    let body = match error {
        None => rust_i18n::t!("preferences.reauthorization_non_native_body"),
        Some(RootReauthorizationError::SourceMissing) => rust_i18n::t!(
            "preferences.reauthorization_source_missing_body",
            old_path = old_path
        ),
        Some(RootReauthorizationError::SamePath) => rust_i18n::t!(
            "preferences.reauthorization_same_path_body",
            path = old_path
        ),
        Some(RootReauthorizationError::OverlappingPath) => rust_i18n::t!(
            "preferences.reauthorization_overlapping_path_body",
            old_path = old_path,
            new_path = new_path.unwrap_or_default()
        ),
        Some(RootReauthorizationError::DuplicateDestination) => rust_i18n::t!(
            "preferences.reauthorization_duplicate_destination_body",
            path = new_path.unwrap_or_default()
        ),
        Some(RootReauthorizationError::PendingRequest) => {
            rust_i18n::t!("preferences.reauthorization_pending_body")
        }
        Some(RootReauthorizationError::UnsupportedPathEncoding) => {
            rust_i18n::t!("preferences.reauthorization_path_encoding_body")
        }
        Some(RootReauthorizationError::InvalidRequestId) => {
            rust_i18n::t!("preferences.reauthorization_internal_error_body")
        }
        Some(RootReauthorizationError::ConfigSaveFailed) => {
            rust_i18n::t!("preferences.reauthorization_save_failed_body")
        }
    };
    let alert = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("preferences.reauthorization_error_heading").as_ref())
        .body(body.as_ref())
        .build();
    alert.add_response("ok", rust_i18n::t!("dialogs.ok").as_ref());
    alert.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST_A: &str = "21c020ca-57df-4fd9-a950-e34fb40a6c1b";
    const REQUEST_B: &str = "3a8bbb48-0c45-4fd4-a645-cd7a5f433b1e";

    fn config_with_paths(paths: &[&str]) -> AppConfig {
        AppConfig {
            library_paths: paths.iter().map(|path| (*path).to_string()).collect(),
            pending_root_reauthorizations: Vec::new(),
            ..AppConfig::default()
        }
    }

    #[test]
    fn schedules_reauthorization_without_replacing_the_configured_root() {
        let mut config = config_with_paths(&["/old"]);

        let result = schedule_root_reauthorization(&mut config, "/old", "/portal/new", REQUEST_A);

        assert_eq!(
            result,
            Ok(RootReauthorizationSchedule::Scheduled {
                request_id: REQUEST_A.to_string()
            })
        );
        assert_eq!(config.library_paths, ["/old"]);
        assert_eq!(
            config.pending_root_reauthorizations,
            [PendingRootReauthorization {
                request_id: REQUEST_A.to_string(),
                old_path: "/old".to_string(),
                new_path: "/portal/new".to_string(),
            }]
        );
    }

    #[test]
    fn pending_request_cannot_be_superseded_while_engine_may_be_processing_it() {
        let mut config = config_with_paths(&["/old"]);
        config.pending_root_reauthorizations = vec![PendingRootReauthorization {
            request_id: REQUEST_A.to_string(),
            old_path: "/old".to_string(),
            new_path: "/portal/first".to_string(),
        }];
        let original = config.pending_root_reauthorizations.clone();

        let result = schedule_root_reauthorization(&mut config, "/old", "/portal/retry", REQUEST_B);

        assert_eq!(result, Err(RootReauthorizationError::PendingRequest));
        assert_eq!(config.pending_root_reauthorizations.len(), 1);
        assert_eq!(config.pending_root_reauthorizations, original);
        assert_eq!(config.library_paths, ["/old"]);
    }

    #[test]
    fn pending_source_cannot_be_removed_until_exact_rejection_unlocks_it() {
        let mut config = config_with_paths(&["/one", "/two"]);
        config.pending_root_reauthorizations = vec![
            PendingRootReauthorization {
                request_id: REQUEST_A.to_string(),
                old_path: "/one".to_string(),
                new_path: "/portal/one".to_string(),
            },
            PendingRootReauthorization {
                request_id: REQUEST_B.to_string(),
                old_path: "/two".to_string(),
                new_path: "/portal/two".to_string(),
            },
        ];

        let original_paths = config.library_paths.clone();
        let original_pending = config.pending_root_reauthorizations.clone();
        assert!(!remove_library_path(&mut config, "/one"));
        assert_eq!(config.library_paths, original_paths);
        assert_eq!(config.pending_root_reauthorizations, original_pending);

        assert!(reject_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/one",
            "/portal/one"
        ));
        assert!(remove_library_path(&mut config, "/one"));

        assert_eq!(config.library_paths, ["/two"]);
        assert_eq!(config.pending_root_reauthorizations.len(), 1);
        assert_eq!(config.pending_root_reauthorizations[0].old_path, "/two");
    }

    #[test]
    fn completion_exact_cas_replaces_root_and_removes_only_matching_intent() {
        let mut config = config_with_paths(&["/old", "/other"]);
        config.pending_root_reauthorizations = vec![
            PendingRootReauthorization {
                request_id: REQUEST_A.to_string(),
                old_path: "/old".to_string(),
                new_path: "/portal/new".to_string(),
            },
            PendingRootReauthorization {
                request_id: REQUEST_B.to_string(),
                old_path: "/other".to_string(),
                new_path: "/portal/other".to_string(),
            },
        ];

        assert!(complete_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/new"
        ));

        assert_eq!(config.library_paths, ["/portal/new", "/other"]);
        assert_eq!(config.pending_root_reauthorizations.len(), 1);
        assert_eq!(
            config.pending_root_reauthorizations[0].request_id,
            REQUEST_B
        );
    }

    #[test]
    fn completion_mismatch_and_ambiguous_state_do_not_mutate_config() {
        let mut config = config_with_paths(&["/old"]);
        config.pending_root_reauthorizations = vec![PendingRootReauthorization {
            request_id: REQUEST_A.to_string(),
            old_path: "/old".to_string(),
            new_path: "/portal/new".to_string(),
        }];

        let original_paths = config.library_paths.clone();
        let original_pending = config.pending_root_reauthorizations.clone();
        assert!(!complete_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/superseded"
        ));
        assert_eq!(config.library_paths, original_paths);
        assert_eq!(config.pending_root_reauthorizations, original_pending);

        config
            .pending_root_reauthorizations
            .push(PendingRootReauthorization {
                request_id: REQUEST_B.to_string(),
                old_path: "/old".to_string(),
                new_path: "/portal/duplicate".to_string(),
            });
        let ambiguous = config.clone();
        assert!(!complete_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/new"
        ));
        assert_eq!(config.library_paths, ambiguous.library_paths);
        assert_eq!(
            config.pending_root_reauthorizations,
            ambiguous.pending_root_reauthorizations
        );
    }

    #[test]
    fn clean_rejection_exact_cas_removes_only_intent_and_keeps_library_paths() {
        let mut config = config_with_paths(&["/old", "/other"]);
        config.pending_root_reauthorizations = vec![
            PendingRootReauthorization {
                request_id: REQUEST_A.to_string(),
                old_path: "/old".to_string(),
                new_path: "/portal/new".to_string(),
            },
            PendingRootReauthorization {
                request_id: REQUEST_B.to_string(),
                old_path: "/other".to_string(),
                new_path: "/portal/other".to_string(),
            },
        ];
        let original_paths = config.library_paths.clone();

        assert!(reject_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/new"
        ));

        assert_eq!(config.library_paths, original_paths);
        assert_eq!(config.pending_root_reauthorizations.len(), 1);
        assert_eq!(
            config.pending_root_reauthorizations[0].request_id,
            REQUEST_B
        );
    }

    #[test]
    fn rejection_mismatch_does_not_mutate_config() {
        let mut config = config_with_paths(&["/old"]);
        config.pending_root_reauthorizations = vec![PendingRootReauthorization {
            request_id: REQUEST_A.to_string(),
            old_path: "/old".to_string(),
            new_path: "/portal/new".to_string(),
        }];
        let original_paths = config.library_paths.clone();
        let original_pending = config.pending_root_reauthorizations.clone();

        assert!(!reject_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/different"
        ));
        assert_eq!(config.library_paths, original_paths);
        assert_eq!(config.pending_root_reauthorizations, original_pending);

        config.library_paths = vec!["/different".to_string()];
        let wrong_roots = config.clone();
        assert!(!reject_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/new"
        ));
        assert_eq!(config.library_paths, wrong_roots.library_paths);
        assert_eq!(
            config.pending_root_reauthorizations,
            wrong_roots.pending_root_reauthorizations
        );

        config.library_paths = vec!["/old".to_string(), "/portal/new".to_string()];
        let duplicate_destination = config.clone();
        assert!(!reject_root_reauthorization(
            &mut config,
            REQUEST_A,
            "/old",
            "/portal/new"
        ));
        assert_eq!(config.library_paths, duplicate_destination.library_paths);
        assert_eq!(
            config.pending_root_reauthorizations,
            duplicate_destination.pending_root_reauthorizations
        );
    }

    #[test]
    fn rejects_configured_and_pending_destination_duplicates_without_mutation() {
        let mut config = config_with_paths(&["/one", "/two", "/three"]);
        schedule_root_reauthorization(&mut config, "/one", "/portal/one", REQUEST_A)
            .expect("schedule first request");
        let snapshot = config.pending_root_reauthorizations.clone();

        assert!(library_path_is_claimed(&config, "/three"));
        assert!(!library_path_is_claimed(&config, "/three/child"));
        assert!(library_path_is_claimed(&config, "/one/child"));
        assert!(library_path_is_claimed(&config, "/portal/one"));
        assert!(library_path_is_claimed(&config, "/portal"));
        assert!(library_path_is_claimed(&config, "/portal/one/child"));
        assert!(!library_path_is_claimed(&config, "/unused"));

        assert_eq!(
            schedule_root_reauthorization(&mut config, "/two", "/three/child", REQUEST_B),
            Err(RootReauthorizationError::DuplicateDestination)
        );
        assert_eq!(
            schedule_root_reauthorization(&mut config, "/two", "/portal", REQUEST_B),
            Err(RootReauthorizationError::DuplicateDestination)
        );
        assert_eq!(config.pending_root_reauthorizations, snapshot);
        assert_eq!(config.library_paths, ["/one", "/two", "/three"]);
    }

    #[test]
    fn overlap_checks_are_component_aware_and_do_not_confuse_prefix_lookalikes() {
        assert!(library_paths_overlap("/music", "/music/album"));
        assert!(library_paths_overlap("/music/album", "/music"));
        assert!(!library_paths_overlap("/music", "/music2"));

        let config = config_with_paths(&["/music", "/other"]);
        assert_eq!(
            validate_root_reauthorization(&config, "/music", "/music/portal"),
            Err(RootReauthorizationError::OverlappingPath)
        );
        assert_eq!(
            validate_root_reauthorization(&config, "/music", "/other/nested"),
            Err(RootReauthorizationError::DuplicateDestination)
        );
        assert_eq!(
            validate_root_reauthorization(&config, "/music", "/music2"),
            Ok(())
        );
    }

    #[test]
    fn rejects_same_path_missing_source_and_invalid_new_request_id() {
        let mut config = config_with_paths(&["/old"]);

        assert_eq!(
            schedule_root_reauthorization(&mut config, "/old", "/old", REQUEST_A),
            Err(RootReauthorizationError::SamePath)
        );
        assert_eq!(
            schedule_root_reauthorization(&mut config, "/missing", "/new", REQUEST_A),
            Err(RootReauthorizationError::SourceMissing)
        );
        assert_eq!(
            schedule_root_reauthorization(&mut config, "/old", "/new", "not-a-uuid"),
            Err(RootReauthorizationError::InvalidRequestId)
        );
        assert!(config.pending_root_reauthorizations.is_empty());
    }

    #[test]
    fn older_config_without_pending_reauthorizations_deserializes_with_empty_default() {
        let config: AppConfig = serde_json::from_str(r#"{"library_paths":["/music"]}"#)
            .expect("deserialize config written before reauthorization support");

        assert_eq!(config.library_paths, ["/music"]);
        assert!(config.pending_root_reauthorizations.is_empty());

        let round_trip = serde_json::to_value(config).expect("serialize current config");
        assert_eq!(
            round_trip["pending_root_reauthorizations"],
            serde_json::json!([])
        );
    }

    #[test]
    fn legacy_column_config_exposes_rating_once_without_losing_user_order() {
        let mut config = AppConfig {
            visible_columns: vec!["Title".to_string(), "Plays".to_string()],
            column_order: vec![
                "Artist".to_string(),
                "Title".to_string(),
                "Plays".to_string(),
                "Format".to_string(),
            ],
            column_schema_version: 0,
            ..AppConfig::default()
        };

        migrate_column_schema(&mut config);
        assert_eq!(
            config.column_order,
            ["Artist", "Title", "Plays", "Rating", "Format"]
        );
        assert_eq!(config.visible_columns, ["Title", "Plays", "Rating"]);
        assert_eq!(config.column_schema_version, CURRENT_COLUMN_SCHEMA_VERSION);

        let once = config.clone();
        migrate_column_schema(&mut config);
        assert_eq!(config.column_order, once.column_order);
        assert_eq!(config.visible_columns, once.visible_columns);
    }

    #[test]
    fn current_column_config_preserves_an_intentionally_hidden_rating() {
        let mut config = AppConfig {
            visible_columns: vec!["Title".to_string()],
            column_order: vec!["Rating".to_string(), "Title".to_string()],
            column_schema_version: CURRENT_COLUMN_SCHEMA_VERSION,
            ..AppConfig::default()
        };

        migrate_column_schema(&mut config);
        assert_eq!(config.visible_columns, ["Title"]);
        assert_eq!(config.column_order, ["Rating", "Title"]);
    }
}
