//! Preferences window — unified settings for library location, browser
//! views, and column visibility.
//!
//! Uses `adw::PreferencesDialog` with a single page: Library Location,
//! Downloads, Browser Views, Visible Columns, Equalizer, Privacy, any
//! integration groups the window supplies (Last.fm), and Import last.

use adw::prelude::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::{info, warn};

use super::persistence::{read_settings_file, write_atomic, SetAsideFile, SettingsRead};

// ── Tracklist columns ───────────────────────────────────────────────────

/// Stable IDs of the tracklist columns, in default display order.
///
/// Each tracklist `ColumnViewColumn` carries its ID in the `id` property.
/// Visibility, order, and sort are persisted by ID, never by display title,
/// so they survive a locale change. The IDs are the English titles that
/// earlier builds persisted, which keeps existing configurations valid.
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

/// Catalog key of a column's display title, or `None` for an unknown ID.
fn column_title_key(id: &str) -> Option<&'static str> {
    Some(match id {
        "#" => "columns.number",
        "Title" => "columns.title",
        "Time" => "columns.time",
        "Artist" => "columns.artist",
        "Album" => "columns.album",
        "Genre" => "columns.genre",
        "Composer" => "columns.composer",
        "Year" => "columns.year",
        "Date Modified" => "columns.date_modified",
        "Bitrate" => "columns.bitrate",
        "Sample Rate" => "columns.sample_rate",
        "Plays" => "columns.plays",
        "Rating" => "columns.rating",
        "Format" => "columns.format",
        _ => return None,
    })
}

/// Display title of a tracklist column in `locale`.
///
/// An unknown ID is returned unchanged.
pub fn column_title(id: &str, locale: &str) -> String {
    column_title_key(id).map_or_else(
        || id.to_string(),
        |key| rust_i18n::t!(key, locale = locale).into_owned(),
    )
}

/// Map a persisted column key to its stable column ID.
///
/// Earlier builds persisted display titles, and the Rating title was already
/// translated, so a non-English session could have stored it localized. Any
/// catalog's title for a known column maps back to that column's ID; an
/// unrecognized key yields `None` and is dropped by callers.
pub fn column_id_from_persisted(key: &str) -> Option<&'static str> {
    if let Some(id) = ALL_COLUMNS.iter().copied().find(|id| *id == key) {
        return Some(id);
    }
    rust_i18n::available_locales!()
        .into_iter()
        .find_map(|locale| {
            ALL_COLUMNS
                .iter()
                .copied()
                .find(|id| column_title(id, locale.as_ref()) == key)
        })
}

/// Rewrite persisted column keys as stable IDs, dropping unknown keys and
/// duplicates while preserving order.
fn normalize_column_keys(keys: &mut Vec<String>) {
    let mut ids: Vec<String> = Vec::with_capacity(keys.len());
    for id in keys.iter().filter_map(|key| column_id_from_persisted(key)) {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    *keys = ids;
}

// ── Persisted configuration ─────────────────────────────────────────────

/// Application configuration persisted to `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Which browser panes are visible.
    #[serde(default)]
    pub browser_views: BrowserViewsConfig,
    /// Which tracklist columns are visible (by stable column ID).
    #[serde(default = "default_visible_columns")]
    pub visible_columns: Vec<String>,
    /// Tracklist column display order (by stable column ID). Persisted
    /// across restarts.
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
    /// Whether `library_paths` was read from config.json rather than
    /// defaulted. Startup forgets tracks outside the configured folders only
    /// when it was, so a missing or unreadable config never discards a
    /// library.
    #[serde(skip)]
    pub library_paths_loaded: bool,
    /// Whether the user has consented to IP-based geolocation for
    /// "Stations Near Me". `None` = not yet asked, `Some(true)` = accepted,
    /// `Some(false)` = declined.
    #[serde(default)]
    pub location_enabled: Option<bool>,
    /// Whether the browser Artist pane groups by Album Artist instead of
    /// the track-level Artist tag. Default: false (group by Artist).
    #[serde(default)]
    pub group_by_album_artist: bool,
    /// Whether the browser Album pane decorates each row with a thumbnail.
    /// Default: false (text-only label) to match the pre-existing look.
    #[serde(default)]
    pub album_pane_artwork: bool,
    /// Thumbnail side length for the browser Album pane, in device pixels.
    /// Default: `Medium` (48 dp). Persisted across restarts.
    #[serde(default)]
    pub album_pane_artwork_size: AlbumArtSize,
    /// Equalizer state for the local output. A malformed value resets only
    /// this field when the file loads.
    #[serde(
        default,
        deserialize_with = "crate::audio::equalizer::EqualizerSettings::deserialize_lenient"
    )]
    pub equalizer: crate::audio::equalizer::EqualizerSettings,
    /// Folder that downloaded remote tracks are saved to. `None` uses
    /// [`crate::download::default_download_dir`].
    #[serde(default)]
    pub download_path: Option<String>,
}

/// Thumbnail side length for the browser Album pane.
///
/// Bounded at the source by the album-art worker's byte cap (32 MiB); the
/// GTK side decodes whatever the worker returns into a square of the
/// selected size, so this knob is layout (not transport) state.
///
/// Persistence uses the stable lowercase tokens from [`AlbumArtSize::as_token`]
/// via the custom `Serialize`/`Deserialize` impls below — not the derived
/// variant names — so a config file written by an older build stays
/// loadable across variant renames and additions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AlbumArtSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl AlbumArtSize {
    /// Side length in device pixels.
    pub const fn pixel_size(self) -> i32 {
        match self {
            Self::Small => 32,
            Self::Medium => 48,
            Self::Large => 72,
        }
    }

    /// Stable persistence token. This is the string written to
    /// `config.json`; new variants must keep older strings recognized
    /// for in-place config migration.
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    /// Parse a previously-persisted token. Also accepts the bare Rust
    /// variant names older builds wrote before the token format existed
    /// (serde's default enum representation), so an in-place upgrade
    /// migrates them instead of dropping the setting. Returns `None`
    /// for unknown values so callers can fall back rather than reject
    /// a config file.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "small" | "Small" => Some(Self::Small),
            "medium" | "Medium" => Some(Self::Medium),
            "large" | "Large" => Some(Self::Large),
            _ => None,
        }
    }
}

impl Serialize for AlbumArtSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for AlbumArtSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        // An unknown token falls back to the default size instead of
        // failing the whole `AppConfig` load: `load_config` treats a
        // deserialize error as "no usable config" and resets every
        // user preference, which is far more destructive than losing
        // one layout knob.
        Ok(Self::from_token(&token).unwrap_or_default())
    }
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
// Four independent persisted view toggles; a bitmask/enum set would not
// simplify the config-file shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserViewsConfig {
    pub genre: bool,
    pub artist: bool,
    pub album: bool,
    /// Root-relative folder browsing pane (#14). Defaults to visible for
    /// fresh configs; the field-level serde default keeps older configs
    /// (written before this pane existed) loading unchanged.
    #[serde(default = "default_folder_view")]
    pub folder: bool,
}

fn default_folder_view() -> bool {
    true
}

impl Default for BrowserViewsConfig {
    fn default() -> Self {
        // All panes visible by default (matches `AppConfig::default`).
        Self {
            genre: true,
            artist: true,
            album: true,
            folder: true,
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
                folder: true,
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
            library_paths_loaded: false,
            location_enabled: None,
            group_by_album_artist: false,
            album_pane_artwork: false,
            album_pane_artwork_size: AlbumArtSize::default(),
            equalizer: crate::audio::equalizer::EqualizerSettings::default(),
            download_path: None,
        }
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

/// Add one library folder unless it is already configured or reserved,
/// saving the change before publishing it. Returns whether it was added; the
/// running engine scans it from the next start.
pub fn add_library_path(config: &std::cell::RefCell<AppConfig>, path: &str) -> bool {
    let mut cfg = config.borrow_mut();
    if library_path_is_claimed(&cfg, path) {
        return false;
    }
    let mut candidate = cfg.clone();
    candidate.library_paths.push(path.to_string());
    if !save_config(&candidate) {
        return false;
    }
    *cfg = candidate;
    info!(path, "Library folder added");
    true
}

/// The folder downloaded tracks are saved to.
pub fn download_dir(config: &AppConfig) -> Option<std::path::PathBuf> {
    config
        .download_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(crate::download::default_download_dir)
}

/// Make the download folder part of the library unless a configured folder
/// already contains it. Returns whether it was added.
pub fn add_download_library_path(
    config: &std::cell::RefCell<AppConfig>,
    folder: &std::path::Path,
) -> bool {
    let covered = config
        .borrow()
        .library_paths
        .iter()
        .any(|root| folder.starts_with(root));
    !covered
        && folder
            .to_str()
            .is_some_and(|path| add_library_path(config, path))
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
    crate::paths::data_dir().map(|d| d.join("tributary").join("config.json"))
}

/// Load the configuration from disk, falling back to defaults.
///
/// An unreadable or unparsable `config.json` is moved aside (see
/// [`read_settings_file`]) and reported through the second value, so the
/// next save cannot silently overwrite the user's library folders and
/// pending reauthorizations. Defaults loaded that way leave
/// `library_paths_loaded` false.
pub fn load_config() -> (AppConfig, Option<SetAsideFile>) {
    config_path().map_or_else(
        || (AppConfig::default(), None),
        |path| load_config_from(&path),
    )
}

fn load_config_from(path: &std::path::Path) -> (AppConfig, Option<SetAsideFile>) {
    match read_settings_file(path, parse_config) {
        SettingsRead::Parsed(config) => (config, None),
        SettingsRead::Missing => (AppConfig::default(), None),
        SettingsRead::Unreadable { set_aside } => (AppConfig::default(), set_aside),
    }
}

/// Parse `config.json`.
///
/// Handles migration from the legacy `library_path` (single string)
/// format. We parse to a `serde_json::Value` first and rename the key
/// programmatically rather than doing a textual `String::replace`,
/// which would corrupt user-supplied values that happened to contain
/// the literal substring `"library_path"`.
fn parse_config(raw: &str) -> Result<AppConfig, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(raw)?;

    if let Some(obj) = value.as_object_mut() {
        if !obj.contains_key("library_paths") {
            if let Some(legacy) = obj.remove("library_path") {
                obj.insert("library_paths".to_string(), legacy);
            }
        }
    }
    let library_paths_loaded = value
        .as_object()
        .is_some_and(|obj| obj.contains_key("library_paths"));

    let mut config = serde_json::from_value::<AppConfig>(value)?;
    config.library_paths_loaded = library_paths_loaded;
    migrate_column_schema(&mut config);
    Ok(config)
}

/// Normalize persisted column keys to stable IDs, then expose each newly
/// introduced column exactly once for established profiles.
///
/// A version marker distinguishes an old profile that could not mention
/// Rating from a current profile where the user intentionally hid or
/// reordered it.
fn migrate_column_schema(config: &mut AppConfig) {
    normalize_column_keys(&mut config.visible_columns);
    normalize_column_keys(&mut config.column_order);
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
/// depends on persistence avoid claiming success, while preference toggles
/// rely on the error log.
pub fn save_config(config: &AppConfig) -> bool {
    let Some(path) = config_path() else {
        warn!("Cannot save config.json because the platform data directory is unavailable");
        return false;
    };
    let mut json = match serde_json::to_vec_pretty(config) {
        Ok(json) => json,
        Err(error) => {
            warn!(error = %error, "Failed to serialize config.json");
            return false;
        }
    };
    json.push(b'\n');
    match write_atomic(&path, &json) {
        Ok(()) => true,
        Err(error) => {
            warn!(error = %error, path = %path.display(), "Failed to save config.json");
            false
        }
    }
}

/// How long preference edits wait before one `config.json` write, so a
/// burst of toggles, a Reset, or a slider drag costs a single synchronous
/// write on the GTK thread.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(750);

/// Coalesces preference edits into one delayed `config.json` save.
///
/// Every edit updates the shared config immediately and calls
/// [`ConfigSaveQueue::schedule`]; [`ConfigSaveQueue::flush`] writes a pending
/// save at once and runs when the Preferences dialog or the window closes.
#[derive(Clone)]
pub struct ConfigSaveQueue {
    config: std::rc::Rc<std::cell::RefCell<AppConfig>>,
    pending: std::rc::Rc<std::cell::Cell<bool>>,
    write: std::rc::Rc<dyn Fn(&AppConfig) -> bool>,
}

impl ConfigSaveQueue {
    pub fn new(config: std::rc::Rc<std::cell::RefCell<AppConfig>>) -> Self {
        Self::with_writer(config, std::rc::Rc::new(save_config))
    }

    fn with_writer(
        config: std::rc::Rc<std::cell::RefCell<AppConfig>>,
        write: std::rc::Rc<dyn Fn(&AppConfig) -> bool>,
    ) -> Self {
        Self {
            config,
            pending: std::rc::Rc::default(),
            write,
        }
    }

    /// Save after [`SAVE_DELAY`] unless a save is already scheduled.
    pub fn schedule(&self) {
        if self.request() {
            let queue = self.clone();
            gtk::glib::timeout_add_local_once(SAVE_DELAY, move || queue.flush());
        }
    }

    /// Mark a save as pending. Returns whether this request must arm the
    /// timer, which is only the first request since the last flush.
    fn request(&self) -> bool {
        !self.pending.replace(true)
    }

    /// Write a scheduled save now.
    pub fn flush(&self) {
        if self.pending.replace(false) {
            (self.write)(&self.config.borrow());
        }
    }
}

// ── Preferences window builder ──────────────────────────────────────────

/// The main-window views that Preferences toggles restyle.
#[derive(Clone)]
pub struct LayoutTargets {
    pub column_view: gtk::ColumnView,
    pub browser_box: gtk::Box,
    pub active_source_key: std::rc::Rc<std::cell::RefCell<String>>,
}

impl LayoutTargets {
    /// Radio views show their own station columns and hide the browser. Every
    /// switch away from radio re-applies the saved layout, so while a radio
    /// view is active a toggle only updates the config.
    fn radio_active(&self) -> bool {
        super::radio::is_radio_backend(&self.active_source_key.borrow())
    }

    fn show_columns(&self, visible: &[String]) {
        if !self.radio_active() {
            apply_column_visibility(&self.column_view, visible);
        }
    }

    fn show_browser(&self, views: &BrowserViewsConfig) {
        if !self.radio_active() {
            update_browser_visibility(&self.browser_box, views);
        }
    }
}

/// Build and present the preferences window.
///
/// # Arguments
/// * `parent` — the main application window (for transient-for)
/// * `layout` — the tracklist, browser, and active source the toggles restyle
/// * `config` — current configuration, mutated on changes
/// * `saves` — coalesces the resulting `config.json` writes
/// * `on_album_artist_changed` — invoked when the artist grouping switch flips
/// * `on_album_pane_artwork_changed` — invoked when album artwork turns on or off
/// * `on_album_pane_artwork_size_changed` — invoked when the artwork size changes
/// * `active_output` — the output the equalizer group applies its settings to
/// * `integration_groups` — groups for optional integrations (Last.fm), placed
///   after the app's own settings and before the Import group
#[allow(clippy::too_many_arguments)] // window-owned handles and callbacks the dialog drives
pub fn show_preferences(
    parent: &adw::ApplicationWindow,
    layout: &LayoutTargets,
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
    saves: &ConfigSaveQueue,
    on_album_artist_changed: std::rc::Rc<dyn Fn(bool)>,
    on_album_pane_artwork_changed: std::rc::Rc<dyn Fn(bool)>,
    on_album_pane_artwork_size_changed: std::rc::Rc<dyn Fn(AlbumArtSize)>,
    active_output: &std::rc::Rc<std::cell::RefCell<Box<dyn crate::audio::output::AudioOutput>>>,
    integration_groups: &[adw::PreferencesGroup],
) {
    let prefs_dialog = adw::PreferencesDialog::builder()
        .title(rust_i18n::t!("preferences.title").as_ref())
        .build();
    {
        // Closing the dialog writes any edit still waiting on the delay.
        let saves = saves.clone();
        prefs_dialog.connect_closed(move |_| saves.flush());
    }

    let page = adw::PreferencesPage::new();
    page.add(&library_group(parent, config));
    page.add(&downloads_group(parent, config, saves));
    for group in browser_views_groups(
        config,
        saves,
        layout,
        on_album_artist_changed,
        on_album_pane_artwork_changed,
        on_album_pane_artwork_size_changed,
    ) {
        page.add(&group);
    }

    let cfg = config.borrow();

    // ── Visible Columns group (dense checkbox grid) ─────────────────
    let columns_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.visible_columns").as_ref())
        .build();

    let locale = rust_i18n::locale();
    let column_checks: Vec<(&str, gtk::CheckButton)> = ALL_COLUMNS
        .iter()
        .map(|&col_id| {
            let is_visible = cfg.visible_columns.iter().any(|c| c == col_id);
            let check = grid_check(&column_title(col_id, &locale), is_visible);

            // Wire each column toggle
            let config = config.clone();
            let saves = saves.clone();
            let layout = layout.clone();
            let id = col_id.to_string();
            check.connect_toggled(move |btn| {
                let mut cfg = config.borrow_mut();
                if btn.is_active() {
                    if !cfg.visible_columns.contains(&id) {
                        cfg.visible_columns.push(id.clone());
                    }
                } else {
                    cfg.visible_columns.retain(|c| c != &id);
                }
                layout.show_columns(&cfg.visible_columns);
                saves.schedule();
            });
            (col_id, check)
        })
        .collect();
    let columns_grid = check_grid(column_checks.iter().map(|(_, check)| check));

    // Reset to Defaults button
    let reset_btn = gtk::Button::builder()
        .label(rust_i18n::t!("preferences.reset_to_defaults").as_ref())
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .margin_top(4)
        .build();
    {
        let config = config.clone();
        let saves = saves.clone();
        let layout = layout.clone();
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
            for (id, check) in &checks {
                check.set_active(DEFAULT_VISIBLE.contains(&id.as_str()));
            }
            let cfg = config.borrow();
            layout.show_columns(&cfg.visible_columns);
            apply_column_order(&layout.column_view, &cfg.column_order);
            saves.schedule();
            info!("Column visibility and order reset to defaults");
        });
    }

    columns_group.add(&columns_grid);
    columns_group.add(&reset_btn);
    page.add(&columns_group);
    page.add(&super::equalizer_panel::preferences_group(
        config,
        saves,
        active_output,
    ));
    page.add(&privacy_group(config, saves));
    for group in integration_groups {
        page.add(group);
    }
    page.add(&import_group());

    prefs_dialog.add(&page);
    drop(cfg);

    prefs_dialog.present(Some(parent));
}

/// The Library Location group: one row per library folder, laid out like
/// the Downloads row, and an Add Folder… row closing the list.
///
/// The running library engine reads the configured folders only at startup,
/// so adding or removing a folder shows a restart hint as the group
/// description, and a pending reauthorization shows its own hint from the
/// start.
fn library_group(
    parent: &adw::ApplicationWindow,
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.library_location").as_ref())
        .build();
    if !config.borrow().pending_root_reauthorizations.is_empty() {
        group.set_description(Some(
            rust_i18n::t!("preferences.reauthorization_restart_hint").as_ref(),
        ));
    }
    for lib_path in &config.borrow().library_paths {
        group.add(&library_folder_row(lib_path, config, &group, parent));
    }

    let add_row = adw::ButtonRow::builder()
        .title(rust_i18n::t!("preferences.add_folder").as_ref())
        .start_icon_name("list-add-symbolic")
        .build();
    group.add(&add_row);

    // Add a folder via the file chooser.
    let (parent, config, group_ref) = (parent.clone(), config.clone(), group.downgrade());
    add_row.connect_activated(move |add_row| {
        let Some(group) = group_ref.upgrade() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title(rust_i18n::t!("preferences.select_music_folder").as_ref())
            .modal(true)
            .build();
        let (config, parent_for_result, add_row) =
            (config.clone(), parent.clone(), add_row.clone());
        dialog.select_folder(
            Some(&parent),
            None::<&gtk::gio::Cancellable>,
            move |result| {
                let Some(path) = result.ok().and_then(|folder| folder.path()) else {
                    return;
                };
                let Some(path) = path.to_str() else {
                    warn!("Ignoring a selected library folder with a non-Unicode path");
                    return;
                };
                if !add_library_path(&config, path) {
                    return;
                }
                // A group only appends rows, so Add Folder… moves back below
                // the new folder, keeping the focus it had.
                group.remove(&add_row);
                group.add(&library_folder_row(
                    path,
                    &config,
                    &group,
                    &parent_for_result,
                ));
                group.add(&add_row);
                add_row.grab_focus();
                // The engine won't pick up the new folder until the next
                // launch — tell the user a restart is needed instead of
                // leaving them with an empty library.
                group.set_description(Some(
                    rust_i18n::t!("preferences.library_restart_hint").as_ref(),
                ));
            },
        );
    });
    group
}

/// Checkbox columns per row in the Browser Views and Visible Columns grids.
const CHECK_GRID_COLUMNS: usize = 4;

/// A checkbox that fills its grid cell but keeps its label left-aligned, so
/// the labels line up down each grid column.
fn grid_check(label: &str, active: bool) -> gtk::CheckButton {
    gtk::CheckButton::builder()
        .label(label)
        .active(active)
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build()
}

/// Lay checkboxes out row by row in a homogeneous grid.
///
/// Browser Views and Visible Columns both use this grid, so their checkbox
/// columns line up across the two groups: every column is equal width and
/// the grid fills the group's clamped width, the leftmost column flush with
/// the left edge and the rightmost with the right edge.
fn check_grid<'a>(checks: impl IntoIterator<Item = &'a gtk::CheckButton>) -> gtk::Grid {
    let grid = gtk::Grid::builder()
        .column_homogeneous(true)
        .row_spacing(4)
        .column_spacing(8)
        .hexpand(true)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    for (index, check) in checks.into_iter().enumerate() {
        let column = (index % CHECK_GRID_COLUMNS) as i32;
        let row = (index / CHECK_GRID_COLUMNS) as i32;
        grid.attach(check, column, row, 1, 1);
    }
    grid
}

/// Album artwork sizes in the order the Album artwork dropdown lists them,
/// after Off.
const ALBUM_ARTWORK_SIZES: [AlbumArtSize; 3] = [
    AlbumArtSize::Small,
    AlbumArtSize::Medium,
    AlbumArtSize::Large,
];

/// The Album artwork dropdown position for the saved settings: Off, or the
/// size the artwork is shown at.
fn album_artwork_position(enabled: bool, size: AlbumArtSize) -> u32 {
    ALBUM_ARTWORK_SIZES
        .iter()
        .position(|choice| enabled && *choice == size)
        .map_or(0, |index| index as u32 + 1)
}

/// The size an Album artwork dropdown position selects, or `None` for Off.
fn album_artwork_size_at(position: u32) -> Option<AlbumArtSize> {
    let index = usize::try_from(position.checked_sub(1)?).ok()?;
    ALBUM_ARTWORK_SIZES.get(index).copied()
}

/// One browser pane's visibility flag in the config.
type PaneFlag = fn(&mut BrowserViewsConfig) -> &mut bool;

/// The Browser Views groups: the pane checkboxes, then an untitled group
/// directly below with the Group by Album Artist switch and the Album
/// artwork dropdown.
///
/// A preferences group lists its rows before any other child, so the grid
/// and the rows need a group each for the grid to come first.
fn browser_views_groups(
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
    saves: &ConfigSaveQueue,
    layout: &LayoutTargets,
    on_album_artist_changed: std::rc::Rc<dyn Fn(bool)>,
    on_album_pane_artwork_changed: std::rc::Rc<dyn Fn(bool)>,
    on_album_pane_artwork_size_changed: std::rc::Rc<dyn Fn(AlbumArtSize)>,
) -> [adw::PreferencesGroup; 2] {
    let cfg = config.borrow();
    let mut saved_views = cfg.browser_views.clone();
    let panes: [(&str, PaneFlag); 4] = [
        ("browser.genre", |views| &mut views.genre),
        ("browser.artist", |views| &mut views.artist),
        ("browser.album", |views| &mut views.album),
        ("browser.folder", |views| &mut views.folder),
    ];
    let pane_checks = panes.map(|(key, flag)| {
        let check = grid_check(rust_i18n::t!(key).as_ref(), *flag(&mut saved_views));
        let (config, saves, layout) = (config.clone(), saves.clone(), layout.clone());
        check.connect_toggled(move |btn| {
            let mut cfg = config.borrow_mut();
            *flag(&mut cfg.browser_views) = btn.is_active();
            layout.show_browser(&cfg.browser_views);
            saves.schedule();
        });
        check
    });
    let panes_group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.browser_views").as_ref())
        .build();
    panes_group.add(&check_grid(&pane_checks));

    let album_artist = adw::SwitchRow::builder()
        .title(rust_i18n::t!("preferences.group_by_album_artist").as_ref())
        .active(cfg.group_by_album_artist)
        .build();
    {
        let (config, saves) = (config.clone(), saves.clone());
        album_artist.connect_active_notify(move |row| {
            let active = row.is_active();
            config.borrow_mut().group_by_album_artist = active;
            saves.schedule();
            on_album_artist_changed(active);
        });
    }

    let choices = [
        rust_i18n::t!("browser.album_artwork_off"),
        rust_i18n::t!("browser.album_artwork_size_small"),
        rust_i18n::t!("browser.album_artwork_size_medium"),
        rust_i18n::t!("browser.album_artwork_size_large"),
    ];
    let artwork = adw::ComboRow::builder()
        .title(rust_i18n::t!("browser.album_artwork").as_ref())
        .model(&gtk::StringList::new(
            &choices.each_ref().map(AsRef::as_ref),
        ))
        .selected(album_artwork_position(
            cfg.album_pane_artwork,
            cfg.album_pane_artwork_size,
        ))
        .build();
    {
        // Off turns the artwork off and keeps the size for next time; a size
        // turns the artwork on at that size. The browser owns the pane
        // rebuild, so each change goes through its callback.
        let (config, saves) = (config.clone(), saves.clone());
        artwork.connect_selected_notify(move |row| {
            if row.selected() == gtk::INVALID_LIST_POSITION {
                return;
            }
            let size = album_artwork_size_at(row.selected());
            let (size_changed, enabled_changed) = {
                let mut cfg = config.borrow_mut();
                let size_changed = size.is_some_and(|size| size != cfg.album_pane_artwork_size);
                if let Some(size) = size {
                    cfg.album_pane_artwork_size = size;
                }
                let enabled_changed = cfg.album_pane_artwork != size.is_some();
                cfg.album_pane_artwork = size.is_some();
                (size_changed, enabled_changed)
            };
            saves.schedule();
            // The size goes first, so turning the artwork on decodes
            // thumbnails only at the chosen size.
            if let Some(size) = size.filter(|_| size_changed) {
                on_album_pane_artwork_size_changed(size);
            }
            if enabled_changed {
                on_album_pane_artwork_changed(size.is_some());
            }
        });
    }
    let rows_group = adw::PreferencesGroup::new();
    rows_group.add(&album_artist);
    rows_group.add(&artwork);
    [panes_group, rows_group]
}

/// The Import group, last on the page: bringing in another player's library
/// is a one-time step rather than a setting.
fn import_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.import").as_ref())
        .build();
    group.add(
        &adw::ButtonRow::builder()
            .title(rust_i18n::t!("rhythmbox_migration.menu_action").as_ref())
            .start_icon_name("document-open-symbolic")
            // Reuse the window action so the Preferences entry follows the
            // same admission, shutdown, and migration-dialog path as the
            // former menu item.
            .action_name("win.migrate-rhythmbox")
            .build(),
    );
    group
}

/// The Downloads group: where downloaded remote tracks are saved.
fn downloads_group(
    parent: &adw::ApplicationWindow,
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
    saves: &ConfigSaveQueue,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.downloads").as_ref())
        .build();
    let folder_label = |config: &AppConfig| {
        download_dir(config).map_or_else(String::new, |folder| folder.display().to_string())
    };
    let row = adw::ActionRow::builder()
        .title(rust_i18n::t!("preferences.download_folder").as_ref())
        .subtitle(folder_label(&config.borrow()))
        .subtitle_selectable(true)
        .build();
    let choose = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .tooltip_text(rust_i18n::t!("preferences.select_download_folder").as_ref())
        .build();
    {
        let (parent, config, saves, row) =
            (parent.clone(), config.clone(), saves.clone(), row.clone());
        choose.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title(rust_i18n::t!("preferences.select_download_folder").as_ref())
                .modal(true)
                .build();
            let (config, saves, row) = (config.clone(), saves.clone(), row.clone());
            dialog.select_folder(
                Some(&parent),
                None::<&gtk::gio::Cancellable>,
                move |result| {
                    let Some(path) = result.ok().and_then(|folder| folder.path()) else {
                        return;
                    };
                    let Some(path) = path.to_str() else {
                        warn!("Ignoring a selected download folder with a non-Unicode path");
                        return;
                    };
                    config.borrow_mut().download_path = Some(path.to_string());
                    row.set_subtitle(&folder_label(&config.borrow()));
                    saves.schedule();
                },
            );
        });
    }
    row.add_suffix(&choose);
    group.add(&row);
    group
}

/// The Privacy group: consent to the IP geolocation behind Stations Near Me.
///
/// Off is an explicit decline, so Stations Near Me stops asking; the consent
/// prompt itself records only an explicit answer.
fn privacy_group(
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
    saves: &ConfigSaveQueue,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("preferences.privacy").as_ref())
        .build();
    let location = adw::SwitchRow::builder()
        .title(rust_i18n::t!("preferences.location_title").as_ref())
        .subtitle(rust_i18n::t!("preferences.location_subtitle").as_ref())
        .active(config.borrow().location_enabled == Some(true))
        .build();
    let config = config.clone();
    let saves = saves.clone();
    location.connect_active_notify(move |row| {
        config.borrow_mut().location_enabled = Some(row.is_active());
        saves.schedule();
    });
    group.add(&location);
    group
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// The `ColumnView`'s columns that carry a stable ID, with their positions.
///
/// The sentinel column that absorbs GTK4's rightmost-column auto-expansion
/// has no ID and is never yielded.
fn identified_columns(
    column_view: &gtk::ColumnView,
) -> impl Iterator<Item = (u32, gtk::ColumnViewColumn, gtk::glib::GString)> {
    let columns = column_view.columns();
    (0..columns.n_items()).filter_map(move |position| {
        let column = columns
            .item(position)
            .and_downcast::<gtk::ColumnViewColumn>()?;
        let id = column.id()?;
        Some((position, column, id))
    })
}

/// Apply column visibility to the `ColumnView` based on the config.
pub fn apply_column_visibility(column_view: &gtk::ColumnView, visible: &[String]) {
    for (_, column, id) in identified_columns(column_view) {
        column.set_visible(visible.iter().any(|v| v == id.as_str()));
    }
}

/// Apply persisted column order to the `ColumnView`.
///
/// Iterates the saved order and moves each column to its target position
/// using `insert_column` (which also removes from the old position).
pub fn apply_column_order(column_view: &gtk::ColumnView, order: &[String]) {
    for (target_pos, wanted) in order.iter().enumerate() {
        let found = identified_columns(column_view).find(|(_, _, id)| id.as_str() == wanted);
        if let Some((current_pos, column, _)) = found {
            if current_pos as usize != target_pos {
                column_view.remove_column(&column);
                column_view.insert_column(target_pos as u32, &column);
            }
        }
    }
}

/// Read the current column order from the `ColumnView` as stable IDs.
pub fn read_column_order(column_view: &gtk::ColumnView) -> Vec<String> {
    identified_columns(column_view)
        .map(|(_, _, id)| id.to_string())
        .collect()
}

/// Update browser pane visibility based on config.
///
/// The browser `Box` has a vertical layout: SearchEntry at the top, then
/// a horizontal panes_box whose children alternate pane and gutter:
/// genre, `.browser-separator`, artist, separator, album, separator,
/// folder.  Panes are recognized structurally — any non-`Separator`
/// child, mapped in that order to the config flags — so this traversal
/// cannot drift out of sync with the layout if gutters are added or
/// removed (indexing raw child positions broke here once the separators
/// became real widgets: disabling Artist hid a separator, disabling
/// Album hid Artist, and Album/Folder could never be hidden).
///
/// A gutter is visible only while a pane survives on each side of it.
/// Panes hidden between two surviving panes hand their separators to the
/// next visible pane, which keeps exactly one (the one adjacent to it)
/// as the gutter and collapses the rest — so with Artist disabled, Genre
/// and Album still get their one 1px gutter instead of touching (spacing
/// is 0; the separator widget is the sole gutter). Separators with no
/// visible pane before or after them — leading the box or trailing the
/// last visible pane — stay hidden so no gutter dangles at an edge. If
/// all four panes are hidden, the entire browser box is hidden.
pub fn update_browser_visibility(browser_box: &gtk::Box, views: &BrowserViewsConfig) {
    // The browser_box layout is: SearchEntry, panes_box (horizontal Box).
    // Find the panes_box (last child, which is a horizontal Box).
    if let Some(panes_box) = browser_box
        .last_child()
        .and_then(|w| w.downcast::<gtk::Box>().ok())
    {
        update_panes_box_visibility(&panes_box, views);
    }
    browser_box.set_visible(views.genre || views.artist || views.album || views.folder);
}

/// Walk the panes_box toggling pane visibility from the config flags, and
/// settle each gutter once a pane survives on both sides of it.
fn update_panes_box_visibility(panes_box: &gtk::Box, views: &BrowserViewsConfig) {
    let mut pane_idx = 0;
    // A gutter needs a visible pane on its left; the box edge is not one,
    // so the traversal starts with no visible pane behind it.
    let mut seen_visible_pane = false;
    // Separators whose right-hand pane has not been reached yet. A hidden
    // pane leaves the queue untouched, so the next visible pane inherits
    // the pending gutters instead of dropping them.
    let mut pending_separators: Vec<gtk::Widget> = Vec::new();
    let mut child = panes_box.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if widget.downcast_ref::<gtk::Separator>().is_some() {
            // A gutter belongs to the pair of panes around it; decide
            // its visibility once the next visible pane is reached.
            pending_separators.push(widget);
            continue;
        }
        // Panes map positionally to the config flags; anything
        // beyond the four known panes stays visible (the old
        // `_ => true` match arm).
        let visible = [views.genre, views.artist, views.album, views.folder]
            .get(pane_idx)
            .copied()
            .unwrap_or(true);
        pane_idx += 1;
        widget.set_visible(visible);
        if visible {
            settle_pending_separators(&mut pending_separators, seen_visible_pane);
            seen_visible_pane = true;
        }
    }
    // Separators after the last visible pane would dangle at the box edge.
    settle_pending_separators(&mut pending_separators, false);
}

/// Show exactly one pending separator — the last queued, the one adjacent
/// to the pane just reached — as the gutter back to the previous visible
/// pane, or hide them all when no visible pane precedes them. Separators
/// carried across hidden panes collapse onto that single gutter, so two
/// surviving panes keep one 1px gap and never a doubled 2px one.
fn settle_pending_separators(pending_separators: &mut Vec<gtk::Widget>, gutter_visible: bool) {
    if let Some(gutter) = pending_separators.pop() {
        gutter.set_visible(gutter_visible);
    }
    for separator in pending_separators.drain(..) {
        separator.set_visible(false);
    }
}

/// One library folder as a row laid out like the Downloads one: the folder's
/// name over its full path, with flat Reauthorize… and remove buttons.
///
/// An empty list is valid (for example on first launch), but a root with an
/// in-flight reauthorization is locked until its exact intent settles.
fn library_folder_row(
    path: &str,
    config: &std::rc::Rc<std::cell::RefCell<AppConfig>>,
    group: &adw::PreferencesGroup,
    parent: &adw::ApplicationWindow,
) -> adw::ActionRow {
    let name = std::path::Path::new(path).file_name().map_or_else(
        || path.to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    // Folder names are plain text, never Pango markup. The labels only take
    // `use-markup` once construction ends, so the text is set afterwards.
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .subtitle_selectable(true)
        .build();
    row.set_title(&name);
    row.set_subtitle(path);

    let reauthorize_label = rust_i18n::t!("preferences.reauthorize_folder");
    let reauthorize_btn = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .tooltip_text(reauthorize_label.as_ref())
        .build();
    reauthorize_btn.update_property(&[gtk::accessible::Property::Label(&reauthorize_label)]);

    let remove_label = rust_i18n::t!("preferences.remove_folder");
    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .tooltip_text(remove_label.as_ref())
        .build();
    remove_btn.update_property(&[gtk::accessible::Property::Label(&remove_label)]);

    let has_pending_request = config
        .borrow()
        .pending_root_reauthorizations
        .iter()
        .any(|pending| pending.old_path == path);
    reauthorize_btn.set_sensitive(!has_pending_request);
    remove_btn.set_sensitive(!has_pending_request);

    row.add_suffix(&reauthorize_btn);
    row.add_suffix(&remove_btn);

    let old_path = path.to_string();
    {
        let config = config.clone();
        let parent = parent.clone();
        let group = group.downgrade();
        let reauthorize_btn_for_state = reauthorize_btn.downgrade();
        let remove_btn_for_state = remove_btn.downgrade();
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
            let group = group.clone();
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
                    let group = group.clone();
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
                                if let Some(group) = group.upgrade() {
                                    group.set_description(Some(
                                        rust_i18n::t!(
                                            "preferences.reauthorization_restart_hint"
                                        )
                                        .as_ref(),
                                    ));
                                }
                                for button in [&reauthorize_btn, &remove_btn] {
                                    if let Some(button) = button.upgrade() {
                                        button.set_sensitive(false);
                                    }
                                }
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

    let config = config.clone();
    let path_owned = path.to_string();
    let group = group.downgrade();
    let row_ref = row.downgrade();
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
        let (Some(group), Some(row)) = (group.upgrade(), row_ref.upgrade()) else {
            return;
        };
        group.remove(&row);
        // The engine keeps watching the removed folder until the next
        // launch — surface a restart hint so the stale tracks aren't
        // mistaken for a bug.
        group.set_description(Some(
            rust_i18n::t!("preferences.library_restart_hint").as_ref(),
        ));
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
    fn only_a_parsed_folder_list_counts_as_loaded() {
        let parse = |raw| parse_config(raw).expect("valid config");
        assert!(parse(r#"{"library_paths":[]}"#).library_paths_loaded);
        let legacy = parse(r#"{"library_path":"/music"}"#);
        assert_eq!(legacy.library_paths, ["/music"]);
        assert!(legacy.library_paths_loaded);
        assert!(!parse("{}").library_paths_loaded);
        assert!(parse_config("{not json").is_err());
        assert!(parse_config(r#"{"library_paths":7}"#).is_err());
        assert!(!AppConfig::default().library_paths_loaded);
    }

    #[test]
    fn a_corrupt_config_is_kept_aside_and_loads_defaults() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.json");
        let corrupt = r#"{"library_paths":["/music/one","/music/two"],"visible"#;
        std::fs::write(&path, corrupt).expect("corrupt config");

        let (config, set_aside) = load_config_from(&path);
        // Defaults loaded this way never count as a configured folder list,
        // so startup cannot forget tracks outside the default folder.
        assert!(!config.library_paths_loaded);
        let set_aside = set_aside.expect("the corrupt config is reported");
        assert_eq!(set_aside.original, path);
        assert!(!path.exists(), "a later save starts a fresh file");
        assert_eq!(
            std::fs::read_to_string(&set_aside.copy).expect("read the kept copy"),
            corrupt
        );

        // A readable config loads normally and reports nothing.
        std::fs::write(&path, r#"{"library_paths":["/music"]}"#).expect("valid config");
        let (config, set_aside) = load_config_from(&path);
        assert!(set_aside.is_none());
        assert!(config.library_paths_loaded);
        assert_eq!(config.library_paths, ["/music"]);
    }

    #[test]
    fn a_burst_of_preference_edits_is_saved_once() {
        let config = std::rc::Rc::new(std::cell::RefCell::new(AppConfig::default()));
        let written = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorder = written.clone();
        let saves = ConfigSaveQueue::with_writer(
            config.clone(),
            std::rc::Rc::new(move |saved: &AppConfig| {
                recorder.borrow_mut().push(saved.visible_columns.clone());
                true
            }),
        );

        // Only the first edit arms the delayed save.
        assert!(saves.request());
        config.borrow_mut().visible_columns = vec!["Title".to_string()];
        assert!(!saves.request());
        config.borrow_mut().visible_columns = vec!["Artist".to_string()];
        assert!(!saves.clone().request());
        assert!(
            written.borrow().is_empty(),
            "nothing is written before the delay"
        );

        saves.flush();
        saves.flush();
        assert_eq!(
            written.borrow().as_slice(),
            [vec!["Artist".to_string()]],
            "one write with the latest settings"
        );
        assert!(saves.request(), "a new edit arms a new save");
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

    #[test]
    fn column_ids_are_the_english_titles_and_every_catalog_translates_them() {
        let locale_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        for locale in rust_i18n::available_locales!() {
            let locale: &str = locale.as_ref();
            let path = locale_dir.join(format!("{locale}.yml"));
            let yaml = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let catalog: serde_yaml::Value = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            for &id in ALL_COLUMNS {
                let key = column_title_key(id).expect("every column ID has a title key");
                let field = key.strip_prefix("columns.").expect("columns.* key");
                let title = column_title(id, locale);
                assert_eq!(
                    catalog["columns"][field].as_str(),
                    Some(title.as_str()),
                    "{locale}.{key} must be present in the catalog"
                );
                if locale == "en" {
                    // Stored configurations hold English titles; the IDs must
                    // stay equal to them for those configurations to apply.
                    assert_eq!(title, id);
                }
                assert_eq!(
                    column_id_from_persisted(&title),
                    Some(id),
                    "{locale} title {title:?} must map back to {id}"
                );
            }
        }
    }

    #[test]
    fn every_catalog_describes_the_location_switch() {
        let locale_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        for locale in rust_i18n::available_locales!() {
            let locale: &str = locale.as_ref();
            let path = locale_dir.join(format!("{locale}.yml"));
            let yaml = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let catalog: serde_yaml::Value = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            for key in ["privacy", "location_title", "location_subtitle"] {
                assert!(
                    catalog["preferences"][key]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "{locale}.preferences.{key} must be present"
                );
            }
            let subtitle = catalog["preferences"]["location_subtitle"]
                .as_str()
                .unwrap_or_default();
            for service in ["ipapi.co", "ipwho.is", "freeipapi.com"] {
                assert!(subtitle.contains(service), "{locale} must name {service}");
            }
        }
    }

    #[test]
    fn album_artwork_dropdown_positions_map_onto_the_saved_settings() {
        for size in ALBUM_ARTWORK_SIZES {
            assert_eq!(
                album_artwork_position(false, size),
                0,
                "off whatever the size"
            );
        }
        for (position, size) in [
            (1, AlbumArtSize::Small),
            (2, AlbumArtSize::Medium),
            (3, AlbumArtSize::Large),
        ] {
            assert_eq!(album_artwork_position(true, size), position);
            assert_eq!(album_artwork_size_at(position), Some(size));
        }
        assert_eq!(album_artwork_size_at(0), None, "Off selects no size");
        assert_eq!(album_artwork_size_at(4), None);
        assert_eq!(album_artwork_size_at(gtk::INVALID_LIST_POSITION), None);
    }

    #[test]
    fn persisted_localized_titles_map_to_stable_ids_and_unknown_keys_drop() {
        let mut config = AppConfig {
            visible_columns: vec!["Bewertung".to_string(), "Title".to_string()],
            column_order: vec![
                "Titel".to_string(),
                "評価".to_string(),
                "Country".to_string(),
                "Title".to_string(),
                "Plays".to_string(),
            ],
            column_schema_version: CURRENT_COLUMN_SCHEMA_VERSION,
            ..AppConfig::default()
        };

        migrate_column_schema(&mut config);
        assert_eq!(config.visible_columns, ["Rating", "Title"]);
        assert_eq!(config.column_order, ["Title", "Rating", "Plays"]);

        let reloaded: AppConfig =
            serde_json::from_value(serde_json::to_value(&config).expect("serialize column config"))
                .expect("deserialize column config");
        assert_eq!(reloaded.visible_columns, config.visible_columns);
        assert_eq!(reloaded.column_order, config.column_order);
    }
}

/// GTK-touching tests for the browser pane/gutter traversal and the
/// tracklist column visibility/order helpers. These are
/// helpers folded into the crate's single consolidated GTK-initializing
/// test (browser.rs `gtk_widget_contracts_hold_on_one_session`) — never
/// standalone `#[test]`s — so only one test ever owns the GTK session.
/// See `ui::widget_test_session`.
// macOS gates out the sole caller (browser.rs's consolidated GTK test
// shares the crate's one GTK session only off-macOS); an ungated copy of
// these symbols is dead code there and fails clippy -D warnings. Mirror
// the caller's gate exactly, as context_menu's helper already does.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::update_browser_visibility;
    use super::BrowserViewsConfig;
    use super::{
        apply_column_order, apply_column_visibility, column_title, identified_columns,
        read_column_order, AlbumArtSize, AppConfig, ConfigSaveQueue, PendingRootReauthorization,
        ALL_COLUMNS,
    };
    use adw::prelude::*;
    use gtk::prelude::{BoxExt, CastNone, ListModelExt, WidgetExt};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// Mirror of `build_browser`'s pane row: SearchEntry stand-in, then
    /// the horizontal panes_box alternating genre, gutter, artist,
    /// gutter, album, gutter, folder. Returns the widgets so assertions
    /// can read each pane's and each gutter's visibility directly.
    struct PaneRow {
        browser_box: gtk::Box,
        panes: [gtk::Box; 4],
        gutters: [gtk::Separator; 3],
    }

    impl PaneRow {
        fn build() -> Self {
            let browser_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            browser_box.append(&gtk::Label::new(Some("search")));
            let panes_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let panes: [gtk::Box; 4] = [
                gtk::Box::new(gtk::Orientation::Horizontal, 0),
                gtk::Box::new(gtk::Orientation::Horizontal, 0),
                gtk::Box::new(gtk::Orientation::Horizontal, 0),
                gtk::Box::new(gtk::Orientation::Horizontal, 0),
            ];
            let gutters: [gtk::Separator; 3] = [
                gtk::Separator::new(gtk::Orientation::Vertical),
                gtk::Separator::new(gtk::Orientation::Vertical),
                gtk::Separator::new(gtk::Orientation::Vertical),
            ];
            panes_box.append(&panes[0]);
            for (i, pane) in panes.iter().enumerate().skip(1) {
                panes_box.append(&gutters[i - 1]);
                panes_box.append(pane);
            }
            browser_box.append(&panes_box);
            Self {
                browser_box,
                panes,
                gutters,
            }
        }
    }

    /// THE reported defect: Genre + Album on, Artist + Folder off.
    /// Exactly one gutter (the one adjacent to Album) survives between
    /// the two survivors; the carried separator from the hidden Artist
    /// collapses instead of hiding both.
    fn interior_hidden_pane_leaves_exactly_one_gutter() {
        let row = PaneRow::build();
        update_browser_visibility(
            &row.browser_box,
            &BrowserViewsConfig {
                genre: true,
                artist: false,
                album: true,
                folder: false,
            },
        );
        assert!(row.panes[0].is_visible(), "genre pane must stay visible");
        assert!(!row.panes[1].is_visible(), "artist pane must hide");
        assert!(row.panes[2].is_visible(), "album pane must stay visible");
        assert!(!row.panes[3].is_visible(), "folder pane must hide");
        assert!(
            !row.gutters[0].is_visible(),
            "gutter carried across the hidden artist must collapse"
        );
        assert!(
            row.gutters[1].is_visible(),
            "genre and album must keep exactly one gutter between them"
        );
        assert!(
            !row.gutters[2].is_visible(),
            "no gutter may dangle after the last visible pane"
        );
    }

    /// All four panes visible: every interior gutter joins two visible
    /// panes and shows.
    fn all_panes_visible_show_every_interior_gutter() {
        let row = PaneRow::build();
        update_browser_visibility(
            &row.browser_box,
            &BrowserViewsConfig {
                genre: true,
                artist: true,
                album: true,
                folder: true,
            },
        );
        for (i, gutter) in row.gutters.iter().enumerate() {
            assert!(
                gutter.is_visible(),
                "gutter {i} joins two visible panes and must show"
            );
        }
    }

    /// Genre + Folder on, Artist + Album off: the two carried gutters
    /// collapse onto the single gutter adjacent to Folder.
    fn disjoint_survivors_collapse_onto_one_gutter() {
        let row = PaneRow::build();
        update_browser_visibility(
            &row.browser_box,
            &BrowserViewsConfig {
                genre: true,
                artist: false,
                album: false,
                folder: true,
            },
        );
        assert!(!row.gutters[0].is_visible());
        assert!(!row.gutters[1].is_visible());
        assert!(row.gutters[2].is_visible(), "exactly one gutter survives");
    }

    /// Only the first pane visible: every gutter lacks a visible pane
    /// on one side, so none may dangle at an edge.
    fn lone_leading_pane_leaves_no_dangling_gutters() {
        let row = PaneRow::build();
        update_browser_visibility(
            &row.browser_box,
            &BrowserViewsConfig {
                genre: true,
                artist: false,
                album: false,
                folder: false,
            },
        );
        for (i, gutter) in row.gutters.iter().enumerate() {
            assert!(!gutter.is_visible(), "leading gutter {i} must not dangle");
        }
    }

    /// Only the last pane visible: same, from the other edge.
    fn lone_trailing_pane_leaves_no_dangling_gutters() {
        let row = PaneRow::build();
        update_browser_visibility(
            &row.browser_box,
            &BrowserViewsConfig {
                genre: false,
                artist: false,
                album: false,
                folder: true,
            },
        );
        for (i, gutter) in row.gutters.iter().enumerate() {
            assert!(
                !gutter.is_visible(),
                "leading gutter {i} before the only visible pane must hide"
            );
        }
    }

    /// All panes hidden: no gutters, and the whole browser box hides.
    fn all_panes_hidden_hide_the_browser_box() {
        let row = PaneRow::build();
        update_browser_visibility(
            &row.browser_box,
            &BrowserViewsConfig {
                genre: false,
                artist: false,
                album: false,
                folder: false,
            },
        );
        for (i, gutter) in row.gutters.iter().enumerate() {
            assert!(!gutter.is_visible(), "gutter {i} must hide");
        }
        assert!(
            !row.browser_box.is_visible(),
            "with every pane hidden the browser box must hide"
        );
    }

    /// The gutter contract around hidden panes: a pane disabled between
    /// two enabled panes must leave exactly one visible 1px gutter between the
    /// survivors (panes spacing is 0 — the separator widget is the sole
    /// gutter), never zero (survivors touching) and never two (a doubled gap).
    /// Leading and trailing gutters must never dangle at a box edge.
    pub fn separator_gutters_join_visible_panes_around_hidden_ones() {
        interior_hidden_pane_leaves_exactly_one_gutter();
        all_panes_visible_show_every_interior_gutter();
        disjoint_survivors_collapse_onto_one_gutter();
        lone_leading_pane_leaves_no_dangling_gutters();
        lone_trailing_pane_leaves_no_dangling_gutters();
        all_panes_hidden_hide_the_browser_box();
    }

    /// The production tracklist, with every column titled as a German
    /// session titles it. Titles are derived from IDs through the same
    /// `column_title` the tracklist uses, with an explicit locale because
    /// rust-i18n's current locale is process-global and tests run in
    /// parallel.
    fn german_tracklist() -> gtk::ColumnView {
        let (admission, _commands) =
            crate::ui::library_commands::LibraryCommandAdmission::channel();
        let (_, _, _, column_view, _, _) = crate::ui::tracklist::build_tracklist(&[], admission);
        for (_, column, id) in identified_columns(&column_view) {
            column.set_title(Some(&column_title(&id, "de")));
        }
        column_view
    }

    fn visible_ids(column_view: &gtk::ColumnView) -> Vec<String> {
        identified_columns(column_view)
            .filter(|(_, column, _)| column.is_visible())
            .map(|(_, _, id)| id.to_string())
            .collect()
    }

    fn column_by_id(column_view: &gtk::ColumnView, wanted: &str) -> gtk::ColumnViewColumn {
        identified_columns(column_view)
            .find(|(_, _, id)| id == wanted)
            .map(|(_, column, _)| column)
            .unwrap_or_else(|| panic!("column {wanted} exists"))
    }

    /// Visibility and order apply by stable ID whatever the display titles
    /// are, and the order read back for persistence is IDs, never titles.
    pub fn column_state_is_keyed_by_id_under_a_non_english_locale() {
        let (admission, _commands) =
            crate::ui::library_commands::LibraryCommandAdmission::channel();
        let (_, _, _, built, _, _) = crate::ui::tracklist::build_tracklist(&[], admission);
        let locale = rust_i18n::locale();
        assert_eq!(read_column_order(&built), ALL_COLUMNS);
        for (_, column, id) in identified_columns(&built) {
            assert_eq!(
                column.title().as_deref(),
                Some(column_title(&id, &locale).as_str())
            );
        }
        let n_columns = built.columns().n_items() as usize;
        assert_eq!(
            n_columns,
            ALL_COLUMNS.len() + 1,
            "only the sentinel column lacks a stable ID"
        );

        let column_view = german_tracklist();
        assert_eq!(
            column_by_id(&column_view, "Rating").title().as_deref(),
            Some("Bewertung")
        );

        let config = AppConfig::default();
        apply_column_visibility(&column_view, &config.visible_columns);
        assert_eq!(visible_ids(&column_view), ALL_COLUMNS);

        let visible = vec!["Title".to_string(), "Rating".to_string()];
        apply_column_visibility(&column_view, &visible);
        assert_eq!(visible_ids(&column_view), visible);

        let mut order = config.column_order;
        order.reverse();
        apply_column_order(&column_view, &order);
        assert_eq!(read_column_order(&column_view), order);
        assert_eq!(
            column_view
                .columns()
                .item(ALL_COLUMNS.len() as u32)
                .and_downcast::<gtk::ColumnViewColumn>()
                .and_then(|column| column.id()),
            None,
            "the sentinel column stays last"
        );
    }

    /// While a radio view is active, Preferences toggles update only the
    /// config: the station columns and the hidden browser stay as the radio
    /// view set them.
    pub fn preference_toggles_leave_the_radio_layout_alone() {
        let column_view = german_tracklist();
        let row = PaneRow::build();
        let layout = super::LayoutTargets {
            column_view: column_view.clone(),
            browser_box: row.browser_box.clone(),
            active_source_key: std::rc::Rc::new(std::cell::RefCell::new(
                crate::ui::radio::TOP_VOTE_SOURCE_KEY.to_string(),
            )),
        };
        crate::ui::radio::apply_radio_columns(&column_view, true);
        row.browser_box.set_visible(false);
        let radio_columns = visible_ids(&column_view);

        let views = BrowserViewsConfig::default();
        layout.show_columns(&["Composer".to_string(), "Plays".to_string()]);
        layout.show_browser(&views);
        assert_eq!(visible_ids(&column_view), radio_columns);
        assert!(
            !row.browser_box.is_visible(),
            "the radio view hides the browser"
        );

        // Outside radio the same toggles apply at once.
        *layout.active_source_key.borrow_mut() = "local".to_string();
        crate::ui::radio::apply_radio_columns(&column_view, false);
        layout.show_columns(&["Composer".to_string(), "Plays".to_string()]);
        layout.show_browser(&views);
        assert_eq!(visible_ids(&column_view), ["Composer", "Plays"]);
        assert!(row.browser_box.is_visible());
    }

    /// Radio mode retitles Artist and Album and hides non-station columns,
    /// but every column keeps its ID, so the persisted order stays valid and
    /// music mode restores the localized music titles.
    pub fn radio_columns_keep_their_ids() {
        let column_view = german_tracklist();
        let locale = rust_i18n::locale();

        crate::ui::radio::apply_radio_columns(&column_view, true);
        assert_eq!(
            visible_ids(&column_view),
            ["Title", "Artist", "Album", "Genre", "Bitrate", "Format"]
        );
        assert_eq!(
            column_by_id(&column_view, "Artist").title().as_deref(),
            Some(rust_i18n::t!("columns.country", locale = &*locale).as_ref())
        );
        assert_eq!(
            column_by_id(&column_view, "Album").title().as_deref(),
            Some(rust_i18n::t!("columns.state_province", locale = &*locale).as_ref())
        );
        assert_eq!(read_column_order(&column_view), ALL_COLUMNS);

        crate::ui::radio::apply_radio_columns(&column_view, false);
        assert_eq!(visible_ids(&column_view), ALL_COLUMNS);
        for (_, column, id) in identified_columns(&column_view) {
            assert_eq!(
                column.title().as_deref(),
                Some(column_title(&id, &locale).as_str())
            );
        }
    }

    /// Every descendant of `root` of type `T`, in tree order.
    fn descendants<T: IsA<gtk::Widget>>(root: &gtk::Widget) -> Vec<T> {
        let mut found = Vec::new();
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(typed) = widget.downcast_ref::<T>() {
                found.push(typed.clone());
            }
            found.extend(descendants::<T>(&widget));
            child = widget.next_sibling();
        }
        found
    }

    /// Library folders are rows like the Downloads one — name, full path,
    /// and flat suffix buttons — closed by the Add Folder… row, and a folder
    /// with a pending reauthorization stays locked.
    pub fn library_folders_are_listed_like_the_downloads_row() {
        let pending = "/media/usb & co/Rock";
        let config = Rc::new(RefCell::new(AppConfig {
            library_paths: vec!["/music/Main".to_string(), pending.to_string()],
            pending_root_reauthorizations: vec![PendingRootReauthorization {
                request_id: "21c020ca-57df-4fd9-a950-e34fb40a6c1b".to_string(),
                old_path: pending.to_string(),
                new_path: "/media/usb/Rock".to_string(),
            }],
            ..AppConfig::default()
        }));
        let parent = adw::ApplicationWindow::builder().build();
        let group = super::library_group(&parent, &config);

        let rows = descendants::<gtk::ListBoxRow>(group.upcast_ref());
        assert_eq!(rows.len(), 3, "two folders and Add Folder…");
        let folders: Vec<adw::ActionRow> = rows[..2]
            .iter()
            .map(|row| row.clone().downcast().expect("folder rows are action rows"))
            .collect();
        let shown: Vec<(String, String)> = folders
            .iter()
            .map(|row| {
                (
                    row.title().into(),
                    row.subtitle().unwrap_or_default().into(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("Main".to_string(), "/music/Main".to_string()),
                ("Rock".to_string(), pending.to_string()),
            ]
        );
        for (row, locked) in folders.iter().zip([false, true]) {
            assert!(!row.uses_markup(), "paths are plain text");
            assert!(row.is_subtitle_selectable());
            let buttons = descendants::<gtk::Button>(row.upcast_ref());
            assert_eq!(buttons.len(), 2, "Reauthorize… and remove");
            for button in &buttons {
                assert!(button.has_css_class("flat"));
                assert_eq!(button.valign(), gtk::Align::Center);
                assert_eq!(button.is_sensitive(), !locked);
            }
        }
        let add = rows[2]
            .clone()
            .downcast::<adw::ButtonRow>()
            .expect("the list ends with Add Folder…");
        assert_eq!(
            add.title().as_str(),
            rust_i18n::t!("preferences.add_folder").as_ref()
        );
        assert_eq!(
            group.description().as_deref(),
            Some(rust_i18n::t!("preferences.reauthorization_restart_hint").as_ref()),
            "a pending reauthorization asks for a restart from the start"
        );
        parent.destroy();
    }

    /// Browser Views puts exactly the four pane checkboxes on one grid row,
    /// then the grouping switch and the artwork dropdown in an untitled group
    /// below, and each control updates the config and calls the browser the
    /// way the checkboxes and size radios did.
    #[allow(clippy::too_many_lines)] // one walk through every control in the group
    pub fn browser_views_rows_drive_the_saved_settings() {
        let config = Rc::new(RefCell::new(AppConfig::default()));
        let writes = Rc::new(Cell::new(0));
        let counter = writes.clone();
        let saves = ConfigSaveQueue::with_writer(
            config.clone(),
            Rc::new(move |_: &AppConfig| {
                counter.set(counter.get() + 1);
                true
            }),
        );
        // Keep one save armed so the edits below never start a save timer.
        assert!(saves.request());
        let panes = PaneRow::build();
        let layout = super::LayoutTargets {
            column_view: german_tracklist(),
            browser_box: panes.browser_box.clone(),
            active_source_key: Rc::new(RefCell::new("local".to_string())),
        };
        let calls: Rc<RefCell<Vec<String>>> = Rc::default();
        let (grouping, artwork, size) = (calls.clone(), calls.clone(), calls.clone());
        let [panes_group, rows_group] = super::browser_views_groups(
            &config,
            &saves,
            &layout,
            Rc::new(move |on| grouping.borrow_mut().push(format!("grouping {on}"))),
            Rc::new(move |on| artwork.borrow_mut().push(format!("artwork {on}"))),
            Rc::new(move |chosen: AlbumArtSize| size.borrow_mut().push(format!("size {chosen:?}"))),
        );

        let grids = descendants::<gtk::Grid>(panes_group.upcast_ref());
        assert_eq!(grids.len(), 1);
        let checks = descendants::<gtk::CheckButton>(grids[0].upcast_ref());
        let placed: Vec<(i32, i32, String)> = checks
            .iter()
            .map(|check| {
                let (column, row, _, _) = grids[0].query_child(check);
                (column, row, check.label().unwrap_or_default().into())
            })
            .collect();
        let expected: Vec<(i32, i32, String)> = [
            "browser.genre",
            "browser.artist",
            "browser.album",
            "browser.folder",
        ]
        .into_iter()
        .enumerate()
        .map(|(column, key)| (column as i32, 0, rust_i18n::t!(key).into_owned()))
        .collect();
        assert_eq!(placed, expected, "one row of pane checkboxes");
        assert!(
            rows_group.title().is_empty(),
            "the rows continue Browser Views"
        );

        checks[3].set_active(false);
        assert!(!config.borrow().browser_views.folder);
        assert!(
            !panes.panes[3].is_visible(),
            "unticking Folder hides its pane"
        );

        let switch = descendants::<adw::SwitchRow>(rows_group.upcast_ref());
        let combo = descendants::<adw::ComboRow>(rows_group.upcast_ref());
        let (switch, combo) = (&switch[0], &combo[0]);
        switch.set_active(true);
        assert!(config.borrow().group_by_album_artist);

        let choices = combo.model().expect("artwork choices");
        let labels: Vec<String> = (0..choices.n_items())
            .map(|position| {
                choices
                    .item(position)
                    .and_downcast::<gtk::StringObject>()
                    .expect("string choice")
                    .string()
                    .into()
            })
            .collect();
        assert_eq!(
            labels,
            [
                "browser.album_artwork_off",
                "browser.album_artwork_size_small",
                "browser.album_artwork_size_medium",
                "browser.album_artwork_size_large",
            ]
            .map(|key| rust_i18n::t!(key).into_owned())
        );
        assert_eq!(combo.selected(), 0, "artwork starts off");

        let artwork_state = || {
            let cfg = config.borrow();
            (cfg.album_pane_artwork, cfg.album_pane_artwork_size)
        };
        combo.set_selected(3);
        assert_eq!(artwork_state(), (true, AlbumArtSize::Large));
        combo.set_selected(0);
        assert_eq!(
            artwork_state(),
            (false, AlbumArtSize::Large),
            "Off keeps the size for next time"
        );
        combo.set_selected(3);
        combo.set_selected(1);
        assert_eq!(artwork_state(), (true, AlbumArtSize::Small));
        assert_eq!(
            calls.borrow().as_slice(),
            [
                "grouping true",
                "size Large",
                "artwork true",
                "artwork false",
                "artwork true",
                "size Small",
            ]
        );

        assert_eq!(writes.get(), 0);
        saves.flush();
        assert_eq!(writes.get(), 1, "every edit shares one pending save");
    }
}
