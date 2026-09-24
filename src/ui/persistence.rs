//! Settings persistence helpers — playback modes, sort state, CSS, HWND,
//! and the atomic reader/writer shared by the JSON settings files.
//!
//! Small single-value files (repeat, shuffle, sort, window geometry) use
//! best-effort writes that ignore errors. The JSON settings files whose loss
//! would reset user configuration (`config.json`, `outputs.json`,
//! `servers.json`) go through [`write_atomic`] and [`read_settings_file`]
//! instead, which report failures and never overwrite an unreadable file.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use tracing::warn;

use crate::ui::header_bar::RepeatMode;

// ── Settings file helpers ───────────────────────────────────────────

fn settings_path(name: &str) -> Option<std::path::PathBuf> {
    crate::paths::data_dir().map(|d| d.join("tributary").join(name))
}

/// Ensure the tributary data directory exists, then write a settings file.
/// Silently ignores errors (best-effort persistence).
fn write_setting(name: &str, content: &str) {
    if let Some(path) = settings_path(name) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, content);
    }
}

// ── Atomic JSON settings files ──────────────────────────────────────

/// Replace `path` with `contents` so that neither a failed write nor a crash
/// leaves a truncated file: the bytes are written and synchronized to a
/// temporary file in the same directory, which then atomically replaces
/// `path`. On any error the previous file is untouched.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "settings file has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    // Syncing the directory makes the rename durable across power loss. The
    // replacement is already visible to this process, so a failure here is
    // logged rather than reported as a failed save.
    #[cfg(unix)]
    if let Err(error) = std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        warn!(%error, path = %parent.display(), "Could not synchronize settings directory");
    }
    Ok(())
}

/// A settings file that could not be read and was moved aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAsideFile {
    pub original: PathBuf,
    pub copy: PathBuf,
}

/// What reading a JSON settings file produced.
#[derive(Debug)]
pub enum SettingsRead<T> {
    /// The file does not exist yet.
    Missing,
    Parsed(T),
    /// The file exists but could not be read or parsed. It was moved to
    /// `set_aside` (`None` when the move failed) so that the next save starts
    /// a fresh file instead of overwriting what the user may want back.
    Unreadable {
        set_aside: Option<SetAsideFile>,
    },
}

/// Read and parse the settings file at `path`, moving it aside when it
/// exists but cannot be read or parsed.
pub fn read_settings_file<T, E: std::fmt::Display>(
    path: &Path,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> SettingsRead<T> {
    let error = match std::fs::read_to_string(path) {
        Ok(raw) => match parse(&raw) {
            Ok(value) => return SettingsRead::Parsed(value),
            Err(error) => error.to_string(),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SettingsRead::Missing;
        }
        Err(error) => error.to_string(),
    };
    let set_aside = match set_aside_unreadable(path) {
        Ok(copy) => {
            warn!(
                %error,
                path = %path.display(),
                copy = %copy.display(),
                "Settings file is unreadable; moved it aside and using defaults"
            );
            Some(SetAsideFile {
                original: path.to_path_buf(),
                copy,
            })
        }
        Err(move_error) => {
            warn!(
                %error,
                %move_error,
                path = %path.display(),
                "Settings file is unreadable and could not be moved aside; using defaults"
            );
            None
        }
    };
    SettingsRead::Unreadable { set_aside }
}

/// Rename `path` to `<name>.corrupt-<UTC timestamp>` beside it.
fn set_aside_unreadable(path: &Path) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "settings file has no name",
            )
        })?
        .to_string_lossy()
        .into_owned();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut copy = path.with_file_name(format!("{name}.corrupt-{stamp}"));
    let mut attempt = 1;
    while copy.exists() {
        copy = path.with_file_name(format!("{name}.corrupt-{stamp}-{attempt}"));
        attempt += 1;
    }
    std::fs::rename(path, &copy)?;
    Ok(copy)
}

/// Localized notice that a settings file was unreadable and moved aside.
pub fn set_aside_notice(file: &SetAsideFile, locale: &str) -> String {
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    rust_i18n::t!(
        "settings.file_set_aside",
        locale = locale,
        file = name(&file.original),
        copy = name(&file.copy)
    )
    .into_owned()
}

// ── Repeat mode ─────────────────────────────────────────────────────

pub fn load_repeat_mode() -> RepeatMode {
    settings_path("repeat")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| match s.trim() {
            "all" => RepeatMode::All,
            "one" => RepeatMode::One,
            _ => RepeatMode::Off,
        })
        .unwrap_or(RepeatMode::Off)
}

pub fn save_repeat_mode(mode: RepeatMode) {
    let s = match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    };
    write_setting("repeat", s);
}

// ── Shuffle ─────────────────────────────────────────────────────────

pub fn load_shuffle() -> bool {
    settings_path("shuffle")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

pub fn save_shuffle(active: bool) {
    write_setting("shuffle", if active { "true" } else { "false" });
}

// ── Column sort state ───────────────────────────────────────────────

pub fn save_sort_state(column_view: &gtk::ColumnView) {
    let Some(sorter) = column_view.sorter() else {
        return;
    };
    let Some(cv_sorter) = sorter.downcast_ref::<gtk::ColumnViewSorter>() else {
        return;
    };

    match cv_sorter.primary_sort_column() {
        Some(column) => {
            let id = column.id().map(|id| id.to_string()).unwrap_or_default();
            let dir = match cv_sorter.primary_sort_order() {
                gtk::SortType::Descending => "desc",
                _ => "asc",
            };
            write_setting("sort", &format!("{id}\n{dir}"));
        }
        None => {
            // No active sort — remove saved state.
            if let Some(path) = settings_path("sort") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

pub fn restore_sort_state(column_view: &gtk::ColumnView) {
    let Some(text) = settings_path("sort").and_then(|p| std::fs::read_to_string(p).ok()) else {
        return;
    };
    let mut lines = text.lines();
    // The first line is a stable column ID, or a display title written by an
    // earlier build.
    let Some(id) = lines
        .next()
        .and_then(super::preferences::column_id_from_persisted)
    else {
        return;
    };
    let order = match lines.next() {
        Some("desc") => gtk::SortType::Descending,
        _ => gtk::SortType::Ascending,
    };

    let columns = column_view.columns();
    for i in 0..columns.n_items() {
        if let Some(col) = columns.item(i) {
            let Some(col) = col.downcast_ref::<gtk::ColumnViewColumn>() else {
                continue;
            };
            if col.id().is_some_and(|column_id| column_id == id) {
                column_view.sort_by_column(Some(col), order);
                return;
            }
        }
    }
}

// ── CSS loading ─────────────────────────────────────────────────────

/// Load the custom CSS from the embedded stylesheet.
pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));

    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("Could not get default display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

// ── Window geometry persistence ─────────────────────────────────

/// Sane bounds for a persisted window dimension. Values outside this range
/// (0, negative, or absurdly large) usually mean a corrupted or hand-edited
/// `window.json`; they are replaced with a default so the app never launches
/// with a degenerate / off-screen window or feeds nonsensical coordinates
/// into the Win32 Snap-Layout hit-test math.
const MIN_WINDOW_DIM: i32 = 200;
const MAX_WINDOW_DIM: i32 = 20_000;
const DEFAULT_WINDOW_WIDTH: i32 = 1400;
const DEFAULT_WINDOW_HEIGHT: i32 = 850;

/// Persisted window size + maximized state.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct WindowGeometry {
    pub width: i32,
    pub height: i32,
    pub is_maximized: bool,
}

/// Save window geometry to disk.
pub fn save_window_geometry(window: &adw::ApplicationWindow) {
    let (width, height) = window.default_size();
    let geo = WindowGeometry {
        width,
        height,
        is_maximized: window.is_maximized(),
    };
    if let Ok(json) = serde_json::to_string(&geo) {
        write_setting("window.json", &json);
    }
}

/// Load persisted window geometry, if any.
///
/// Validates the deserialized dimensions: structurally-valid JSON with
/// out-of-range numbers (0, negative, or enormous) is clamped to a default
/// rather than applied verbatim.
pub fn load_window_geometry() -> Option<WindowGeometry> {
    let mut geo: WindowGeometry = settings_path("window.json")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())?;

    if !(MIN_WINDOW_DIM..=MAX_WINDOW_DIM).contains(&geo.width) {
        geo.width = DEFAULT_WINDOW_WIDTH;
    }
    if !(MIN_WINDOW_DIM..=MAX_WINDOW_DIM).contains(&geo.height) {
        geo.height = DEFAULT_WINDOW_HEIGHT;
    }
    Some(geo)
}

// ── Native window handle extraction ─────────────────────────────

/// Extract the native window handle for `souvlaki`.
#[cfg(target_os = "windows")]
pub fn extract_hwnd(window: &adw::ApplicationWindow) -> Option<*mut std::ffi::c_void> {
    use gtk::prelude::NativeExt;

    let surface = window.surface()?;
    let win32_surface = surface.downcast_ref::<gdk4_win32::Win32Surface>()?;
    let hwnd = win32_surface.handle();
    Some(hwnd.0)
}

#[cfg(not(target_os = "windows"))]
pub fn extract_hwnd(_window: &adw::ApplicationWindow) -> Option<*mut std::ffi::c_void> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_the_file_and_reports_failure() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("nested").join("settings.json");

        write_atomic(&path, b"first").expect("write into a new directory");
        write_atomic(&path, b"second").expect("replace the file");
        assert_eq!(std::fs::read(&path).expect("read back"), b"second");

        // A regular file where the parent directory should be makes the
        // write fail, and the failure is reported rather than swallowed.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");
        assert!(write_atomic(&blocker.join("settings.json"), b"lost").is_err());
    }

    #[test]
    fn missing_and_valid_files_are_read_without_moving_anything() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("settings.json");
        let parse = |raw: &str| serde_json::from_str::<Vec<u32>>(raw);

        assert!(matches!(
            read_settings_file(&path, parse),
            SettingsRead::Missing
        ));
        std::fs::write(&path, "[1, 2]").expect("valid file");
        assert!(matches!(
            read_settings_file(&path, parse),
            SettingsRead::Parsed(values) if values == [1, 2]
        ));
        assert!(path.exists());
    }

    #[test]
    fn an_unparsable_file_is_moved_aside_with_its_bytes_intact() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{truncated").expect("corrupt file");

        let SettingsRead::Unreadable {
            set_aside: Some(first),
        } = read_settings_file(&path, |raw| serde_json::from_str::<Vec<u32>>(raw))
        else {
            panic!("a corrupt file must be reported and moved aside");
        };
        assert_eq!(first.original, path);
        assert!(
            !path.exists(),
            "the corrupt file no longer blocks a fresh save"
        );
        assert_eq!(
            std::fs::read_to_string(&first.copy).expect("read the kept copy"),
            "{truncated"
        );
        let copy_name = first.copy.file_name().unwrap().to_string_lossy();
        assert!(
            copy_name.starts_with("settings.json.corrupt-"),
            "{copy_name}"
        );

        // A second corruption in the same second keeps both copies.
        std::fs::write(&path, "also broken").expect("second corrupt file");
        let SettingsRead::Unreadable {
            set_aside: Some(second),
        } = read_settings_file(&path, |raw| serde_json::from_str::<Vec<u32>>(raw))
        else {
            panic!("the second corrupt file must be moved aside too");
        };
        assert_ne!(first.copy, second.copy);
        assert!(first.copy.exists() && second.copy.exists());
    }

    #[test]
    fn set_aside_notice_is_translated_in_every_catalog() {
        let file = SetAsideFile {
            original: PathBuf::from("/data/tributary/config.json"),
            copy: PathBuf::from("/data/tributary/config.json.corrupt-20260923T101500Z"),
        };
        let english = set_aside_notice(&file, "en");
        for locale in rust_i18n::available_locales!() {
            let text = set_aside_notice(&file, &locale);
            assert!(!text.contains("%{"), "{locale} left a placeholder");
            assert!(text.contains("config.json"), "{locale} names the file");
            assert!(
                text.contains("config.json.corrupt-20260923T101500Z"),
                "{locale} names the kept copy"
            );
            if locale != "en" {
                assert_ne!(text, english, "{locale} fell back to English");
            }
        }
    }
}
