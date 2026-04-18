//! Settings persistence helpers — playback modes, sort state, CSS, HWND.
//!
//! All functions use best-effort file I/O (silently ignore errors) to
//! persist small state values to `<data_dir>/tributary/`.

use adw::prelude::*;

use crate::ui::header_bar::RepeatMode;

// ── Settings file helpers ───────────────────────────────────────────

fn settings_path(name: &str) -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join(name))
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
            let title = column.title().map(|t| t.to_string()).unwrap_or_default();
            let dir = match cv_sorter.primary_sort_order() {
                gtk::SortType::Descending => "desc",
                _ => "asc",
            };
            write_setting("sort", &format!("{title}\n{dir}"));
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
    let Some(title) = lines.next() else { return };
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
            if col.title().is_some_and(|t| t == title) {
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

// ── Native window handle extraction ─────────────────────────────────

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
