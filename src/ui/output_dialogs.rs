//! Audio output persistence (`outputs.json`) and the "Add Output" dialog.
//!
//! Manages saved MPD outputs that appear in the header bar output
//! selector popover.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::persistence::{read_settings_file, write_atomic, SetAsideFile, SettingsRead};

/// A saved audio output entry in `outputs.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedOutput {
    /// Output type: `"mpd"` (extensible to `"airplay"` etc.).
    #[serde(rename = "type")]
    pub output_type: String,
    /// Human-readable display name.
    pub name: String,
    /// Host for MPD connections.
    pub host: String,
    /// Port for MPD connections (typically 6600).
    pub port: u16,
    /// Whether the user confirmed that Tributary exclusively controls this
    /// MPD playback partition. Legacy entries deliberately default to false.
    #[serde(default)]
    pub exclusive_control: bool,
    /// Whether the user opted in to automatic detection of exclusive control
    /// before automatic orphan cleanup. Independent of
    /// `exclusive_control`: a user can confirm exclusive control without
    /// opting in to detection, or opt in to detection without an explicit
    /// confirmation. Legacy entries (and the default for new entries) are
    /// `false` so the conservative "retain orphan unless confirmed" path
    /// is the default.
    #[serde(default)]
    pub detection_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedOutputUpsert {
    Added,
    Upgraded,
    Unchanged,
}

/// Path to `outputs.json`: `<data_dir>/tributary/outputs.json`.
fn outputs_json_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("tributary").join("outputs.json"))
}

/// Load saved outputs from `outputs.json`, returning an empty list when the
/// file is missing or unreadable.
pub fn load_saved_outputs() -> Vec<SavedOutput> {
    load_saved_outputs_reporting().0
}

/// Load saved outputs, also reporting where an unreadable `outputs.json`
/// was moved aside so the next save cannot overwrite it.
pub fn load_saved_outputs_reporting() -> (Vec<SavedOutput>, Option<SetAsideFile>) {
    outputs_json_path().map_or_else(|| (Vec::new(), None), |path| load_saved_outputs_from(&path))
}

fn load_saved_outputs_from(path: &Path) -> (Vec<SavedOutput>, Option<SetAsideFile>) {
    match read_settings_file(path, |raw| serde_json::from_str::<Vec<SavedOutput>>(raw)) {
        SettingsRead::Parsed(outputs) => (outputs, None),
        SettingsRead::Missing => (Vec::new(), None),
        SettingsRead::Unreadable { set_aside } => (Vec::new(), set_aside),
    }
}

/// Atomically replace `outputs.json` with `outputs`.
fn save_outputs_to(path: &Path, outputs: &[SavedOutput]) -> std::io::Result<()> {
    let mut json = serde_json::to_vec_pretty(outputs).map_err(std::io::Error::other)?;
    json.push(b'\n');
    write_atomic(path, &json)
}

fn upsert_saved_output(
    outputs: &mut Vec<SavedOutput>,
    output_type: &str,
    name: &str,
    host: &str,
    port: u16,
    exclusive_control: bool,
    detection_enabled: bool,
) -> SavedOutputUpsert {
    if let Some(existing) = outputs
        .iter_mut()
        .find(|output| output.host == host && output.port == port)
    {
        // Re-adding a legacy endpoint is the explicit migration path. Preserve
        // its existing type, display name, and detection opt-in so the
        // already-rendered selector row remains an exact representation of
        // the saved entry. Detection is opt-in per entry, so flipping it
        // off is not a default migration.
        if exclusive_control && !existing.exclusive_control {
            existing.exclusive_control = true;
            if detection_enabled {
                existing.detection_enabled = true;
            }
            return SavedOutputUpsert::Upgraded;
        }
        if detection_enabled && !existing.detection_enabled {
            existing.detection_enabled = true;
            return SavedOutputUpsert::Upgraded;
        }
        return SavedOutputUpsert::Unchanged;
    }

    outputs.push(SavedOutput {
        output_type: output_type.to_string(),
        name: name.to_string(),
        host: host.to_string(),
        port,
        exclusive_control,
        detection_enabled,
    });
    SavedOutputUpsert::Added
}

/// Add or explicitly upgrade an output in `outputs.json` (dedup by host:port).
///
/// An error means nothing was saved: the output is not available after a
/// restart and must not be shown as added.
pub fn add_saved_output(
    output_type: &str,
    name: &str,
    host: &str,
    port: u16,
    exclusive_control: bool,
    detection_enabled: bool,
) -> std::io::Result<SavedOutputUpsert> {
    let path = outputs_json_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the platform data directory is unavailable",
        )
    })?;
    add_saved_output_at(
        &path,
        output_type,
        name,
        host,
        port,
        exclusive_control,
        detection_enabled,
    )
}

fn add_saved_output_at(
    path: &Path,
    output_type: &str,
    name: &str,
    host: &str,
    port: u16,
    exclusive_control: bool,
    detection_enabled: bool,
) -> std::io::Result<SavedOutputUpsert> {
    let (mut outputs, _) = load_saved_outputs_from(path);
    let outcome = upsert_saved_output(
        &mut outputs,
        output_type,
        name,
        host,
        port,
        exclusive_control,
        detection_enabled,
    );
    if outcome != SavedOutputUpsert::Unchanged {
        save_outputs_to(path, &outputs)?;
        info!(host = %host, port, ?outcome, "Output saved to outputs.json");
    }
    Ok(outcome)
}

fn probe_failed_message(locale: &str, host: &str, port: u16) -> String {
    rust_i18n::t!(
        "dialogs.output_probe_failed",
        locale = locale,
        host = host,
        port = port
    )
    .into_owned()
}

fn save_failed_message(locale: &str, name: &str) -> String {
    rust_i18n::t!("dialogs.output_save_failed", locale = locale, name = name).into_owned()
}

/// Show `message` as plain text; host and output names are user input.
fn show_toast(toasts: &adw::ToastOverlay, message: &str) {
    toasts.add_toast(
        adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .build(),
    );
}

fn exclusive_control_warning(locale: &str) -> String {
    rust_i18n::t!("dialogs.output_exclusive_control_warning", locale = locale).into_owned()
}

fn exclusive_control_confirmation(locale: &str) -> String {
    rust_i18n::t!(
        "dialogs.output_exclusive_control_confirmation",
        locale = locale
    )
    .into_owned()
}

fn detection_warning(locale: &str) -> String {
    rust_i18n::t!("dialogs.output_detection_warning", locale = locale).into_owned()
}

fn detection_confirmation(locale: &str) -> String {
    rust_i18n::t!("dialogs.output_detection_confirmation", locale = locale).into_owned()
}

/// Remove an output from `outputs.json` by host:port.
#[allow(dead_code)]
pub fn remove_saved_output(host: &str, port: u16) -> std::io::Result<()> {
    let Some(path) = outputs_json_path() else {
        return Ok(());
    };
    let (mut outputs, _) = load_saved_outputs_from(&path);
    let before = outputs.len();
    outputs.retain(|o| !(o.host == host && o.port == port));
    if outputs.len() != before {
        save_outputs_to(&path, &outputs)?;
        info!(host = %host, port, "Output removed from outputs.json");
    }
    Ok(())
}

/// Present the "Add Output" dialog.
///
/// Currently supports MPD outputs only.  The dialog collects a display
/// name, host, and port, then probes the MPD server on a background
/// thread to validate connectivity before saving. The dialog has closed by
/// the time the probe finishes, so a failed probe or save is reported as a
/// toast.
pub fn show_add_output_dialog(
    window: &adw::ApplicationWindow,
    output_list: &gtk::ListBox,
    toasts: &adw::ToastOverlay,
) {
    use adw::prelude::*;
    use gtk::glib;

    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("dialogs.add_output_heading").as_ref())
        .body(rust_i18n::t!("dialogs.add_output_body").as_ref())
        .close_response("cancel")
        .default_response("add")
        .build();

    dialog.add_response("cancel", rust_i18n::t!("dialogs.cancel").as_ref());
    dialog.add_response("add", rust_i18n::t!("dialogs.add").as_ref());
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);

    let name_entry = gtk::Entry::builder()
        .placeholder_text(rust_i18n::t!("dialogs.output_name_placeholder").as_ref())
        .text("MPD")
        .activates_default(true)
        .hexpand(true)
        .build();

    let host_entry = gtk::Entry::builder()
        .placeholder_text("localhost")
        .text("localhost")
        .activates_default(true)
        .hexpand(true)
        .build();

    let port_spin = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    port_spin.set_value(6600.0);
    port_spin.set_hexpand(true);

    let exclusive_warning = gtk::Label::builder()
        .label(exclusive_control_warning(&rust_i18n::locale()))
        .halign(gtk::Align::Start)
        .wrap(true)
        .max_width_chars(52)
        .build();
    let exclusive_confirmation_label = gtk::Label::builder()
        .label(exclusive_control_confirmation(&rust_i18n::locale()))
        .halign(gtk::Align::Start)
        .wrap(true)
        .max_width_chars(52)
        .xalign(0.0)
        .build();
    let exclusive_confirmation = gtk::CheckButton::builder()
        .child(&exclusive_confirmation_label)
        .halign(gtk::Align::Start)
        .build();

    let detection_warning = gtk::Label::builder()
        .label(detection_warning(&rust_i18n::locale()))
        .halign(gtk::Align::Start)
        .wrap(true)
        .max_width_chars(52)
        .build();
    let detection_confirmation_label = gtk::Label::builder()
        .label(detection_confirmation(&rust_i18n::locale()))
        .halign(gtk::Align::Start)
        .wrap(true)
        .max_width_chars(52)
        .xalign(0.0)
        .build();
    let detection_confirmation = gtk::CheckButton::builder()
        .child(&detection_confirmation_label)
        .halign(gtk::Align::Start)
        .build();

    // Use a GtkGrid for consistent label alignment.
    let grid = gtk::Grid::builder()
        .row_spacing(12)
        .column_spacing(16)
        .margin_top(12)
        .margin_bottom(4)
        .margin_start(8)
        .margin_end(8)
        .build();

    let name_label = gtk::Label::builder()
        .label(rust_i18n::t!("dialogs.output_name").as_ref())
        .halign(gtk::Align::End)
        .build();
    let host_label = gtk::Label::builder()
        .label(rust_i18n::t!("dialogs.output_host").as_ref())
        .halign(gtk::Align::End)
        .build();
    let port_label = gtk::Label::builder()
        .label(rust_i18n::t!("dialogs.output_port").as_ref())
        .halign(gtk::Align::End)
        .build();

    grid.attach(&name_label, 0, 0, 1, 1);
    grid.attach(&name_entry, 1, 0, 1, 1);
    grid.attach(&host_label, 0, 1, 1, 1);
    grid.attach(&host_entry, 1, 1, 1, 1);
    grid.attach(&port_label, 0, 2, 1, 1);
    grid.attach(&port_spin, 1, 2, 1, 1);
    grid.attach(&exclusive_warning, 0, 3, 2, 1);
    grid.attach(&exclusive_confirmation, 0, 4, 2, 1);
    grid.attach(&detection_warning, 0, 5, 2, 1);
    grid.attach(&detection_confirmation, 0, 6, 2, 1);

    dialog.set_extra_child(Some(&grid));
    dialog.set_response_enabled("add", false);
    let dialog_for_confirmation = dialog.clone();
    exclusive_confirmation.connect_toggled(move |confirmation| {
        dialog_for_confirmation.set_response_enabled("add", confirmation.is_active());
    });

    let output_list = output_list.clone();
    let toasts = toasts.clone();
    let name_entry_c = name_entry.clone();
    let host_entry_c = host_entry.clone();
    let port_spin_c = port_spin.clone();
    let exclusive_confirmation_c = exclusive_confirmation.clone();
    let detection_confirmation_c = detection_confirmation.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response != "add" || !exclusive_confirmation_c.is_active() {
            return;
        }

        let name = name_entry_c.text().to_string().trim().to_string();
        let host = host_entry_c.text().to_string().trim().to_string();
        let port = port_spin_c.value() as u16;
        let detection_enabled = detection_confirmation_c.is_active();

        if name.is_empty() || host.is_empty() {
            return;
        }

        // Probe on a background thread to avoid blocking the UI.
        let (probe_tx, probe_rx) = async_channel::bounded::<Result<String, String>>(1);
        let probe_host = host.clone();
        std::thread::spawn(move || {
            let result = crate::audio::mpd_output::MpdOutput::probe(&probe_host, port);
            let _ = probe_tx.send_blocking(result);
        });

        let output_list = output_list.clone();
        let toasts = toasts.clone();
        glib::MainContext::default().spawn_local(async move {
            let Ok(result) = probe_rx.recv().await else {
                return;
            };
            let version = match result {
                Ok(version) => version,
                Err(e) => {
                    warn!(
                        host = %host,
                        port,
                        error = %e,
                        "MPD probe failed — output not added"
                    );
                    show_toast(
                        &toasts,
                        &probe_failed_message(&rust_i18n::locale(), &host, port),
                    );
                    return;
                }
            };
            info!(
                name = %name,
                host = %host,
                port,
                version = %version,
                detection_enabled,
                "MPD output probed successfully"
            );
            match add_saved_output("mpd", &name, &host, port, true, detection_enabled) {
                // A legacy endpoint is upgraded in place; its row is
                // already present and retains its saved display name.
                Ok(SavedOutputUpsert::Added) => {
                    let row =
                        super::header_bar::build_output_row(&name, "network-server-symbolic", false);
                    output_list.append(&row);
                }
                Ok(SavedOutputUpsert::Upgraded | SavedOutputUpsert::Unchanged) => {}
                Err(error) => {
                    warn!(%error, host = %host, port, "Failed to save outputs.json — output not added");
                    show_toast(&toasts, &save_failed_message(&rust_i18n::locale(), &name));
                }
            }
        });
    });

    dialog.present(Some(window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_output_defaults_to_unconfirmed_and_approved_output_serializes_mode() {
        let legacy = r#"[{"type":"mpd","name":"Legacy","host":"mpd.local","port":6600}]"#;
        let mut outputs: Vec<SavedOutput> = serde_json::from_str(legacy).expect("legacy JSON");
        assert!(!outputs[0].exclusive_control);
        assert!(!outputs[0].detection_enabled);

        assert_eq!(
            upsert_saved_output(
                &mut outputs,
                "mpd",
                "Replacement",
                "mpd.local",
                6600,
                true,
                true
            ),
            SavedOutputUpsert::Upgraded
        );
        let serialized = serde_json::to_string(&outputs).expect("approved JSON");
        assert!(serialized.contains(r#""exclusive_control":true"#));
        assert!(serialized.contains(r#""detection_enabled":true"#));
    }

    #[test]
    fn detection_only_opt_in_upgrades_without_changing_exclusive_confirmation() {
        let mut outputs = vec![SavedOutput {
            output_type: "mpd".to_string(),
            name: "Living Room".to_string(),
            host: "mpd.local".to_string(),
            port: 6600,
            exclusive_control: false,
            detection_enabled: false,
        }];
        assert_eq!(
            upsert_saved_output(
                &mut outputs,
                "mpd",
                "Living Room",
                "mpd.local",
                6600,
                false,
                true
            ),
            SavedOutputUpsert::Upgraded
        );
        assert!(outputs[0].detection_enabled);
        assert!(!outputs[0].exclusive_control);
    }

    #[test]
    fn endpoint_upsert_upgrades_in_place_without_renaming_or_dropping_siblings() {
        let mut outputs = vec![
            SavedOutput {
                output_type: "mpd".to_string(),
                name: "Living Room".to_string(),
                host: "mpd.local".to_string(),
                port: 6600,
                exclusive_control: false,
                detection_enabled: false,
            },
            SavedOutput {
                output_type: "mpd".to_string(),
                name: "Office".to_string(),
                host: "office.local".to_string(),
                port: 6601,
                exclusive_control: true,
                detection_enabled: false,
            },
        ];
        let sibling = outputs[1].clone();

        assert_eq!(
            upsert_saved_output(
                &mut outputs,
                "mpd",
                "Renamed",
                "mpd.local",
                6600,
                true,
                false
            ),
            SavedOutputUpsert::Upgraded
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "Living Room");
        assert!(outputs[0].exclusive_control);
        assert!(!outputs[0].detection_enabled);
        assert_eq!(outputs[1], sibling);
        assert_eq!(
            upsert_saved_output(
                &mut outputs,
                "mpd",
                "Renamed",
                "mpd.local",
                6600,
                true,
                false
            ),
            SavedOutputUpsert::Unchanged
        );
        assert_eq!(outputs.len(), 2);
    }

    #[test]
    fn adding_an_output_saves_it_and_reports_the_outcome() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("tributary").join("outputs.json");

        let added = add_saved_output_at(&path, "mpd", "Den", "den.local", 6600, true, false)
            .expect("save to a writable directory");
        assert_eq!(added, SavedOutputUpsert::Added);
        let again = add_saved_output_at(&path, "mpd", "Den", "den.local", 6600, true, false)
            .expect("re-adding the same endpoint");
        assert_eq!(again, SavedOutputUpsert::Unchanged);

        let (saved, set_aside) = load_saved_outputs_from(&path);
        assert!(set_aside.is_none());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "Den");
        assert!(saved[0].exclusive_control);
    }

    #[test]
    fn a_failed_write_is_reported_instead_of_claiming_the_output_was_added() {
        let dir = tempfile::tempdir().expect("temporary directory");
        // A regular file where the data directory should be makes the save
        // fail.
        let blocker = dir.path().join("tributary");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");

        let result = add_saved_output_at(
            &blocker.join("outputs.json"),
            "mpd",
            "Den",
            "den.local",
            6600,
            true,
            false,
        );
        assert!(result.is_err(), "got {result:?}");
    }

    #[test]
    fn a_corrupt_outputs_file_is_kept_aside_instead_of_overwritten() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("outputs.json");
        let corrupt = r#"[{"type":"mpd","name":"Living Room","host":"mpd.local","#;
        std::fs::write(&path, corrupt).expect("corrupt outputs file");

        let added = add_saved_output_at(&path, "mpd", "Den", "den.local", 6600, true, false)
            .expect("save after setting the corrupt file aside");
        assert_eq!(added, SavedOutputUpsert::Added);

        let (saved, _) = load_saved_outputs_from(&path);
        assert_eq!(saved.len(), 1, "only the new output is in the fresh file");
        let copies: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list the data directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|entry| entry != &path)
            .collect();
        assert_eq!(copies.len(), 1, "the corrupt file was kept: {copies:?}");
        assert_eq!(
            std::fs::read_to_string(&copies[0]).expect("read the kept copy"),
            corrupt
        );
    }

    #[test]
    fn add_output_failures_are_localized_everywhere() {
        let english_probe = probe_failed_message("en", "mpd.local", 6601);
        let english_save = save_failed_message("en", "Den <b>");
        for locale in rust_i18n::available_locales!() {
            let probe = probe_failed_message(&locale, "mpd.local", 6601);
            let save = save_failed_message(&locale, "Den <b>");
            assert!(!probe.contains("%{") && !save.contains("%{"), "{locale}");
            assert!(
                probe.contains("mpd.local") && probe.contains("6601"),
                "{locale}"
            );
            assert!(save.contains("Den <b>"), "{locale}");
            if locale != "en" {
                assert_ne!(probe, english_probe, "{locale} probe fallback");
                assert_ne!(save, english_save, "{locale} save fallback");
            }
        }
    }

    #[test]
    fn exclusive_control_warning_and_confirmation_are_localized_everywhere() {
        let english_warning = exclusive_control_warning("en");
        let english_confirmation = exclusive_control_confirmation("en");
        for locale in rust_i18n::available_locales!() {
            let warning = exclusive_control_warning(&locale);
            let confirmation = exclusive_control_confirmation(&locale);
            assert!(!warning.is_empty(), "{locale} warning");
            assert!(!confirmation.is_empty(), "{locale} confirmation");
            if locale != "en" {
                assert_ne!(warning, english_warning, "{locale} warning fallback");
                assert_ne!(
                    confirmation, english_confirmation,
                    "{locale} confirmation fallback"
                );
            }
        }
    }

    #[test]
    fn detection_warning_and_confirmation_are_localized_everywhere() {
        let english_warning = detection_warning("en");
        let english_confirmation = detection_confirmation("en");
        for locale in rust_i18n::available_locales!() {
            let warning = detection_warning(&locale);
            let confirmation = detection_confirmation(&locale);
            assert!(!warning.is_empty(), "{locale} warning");
            assert!(!confirmation.is_empty(), "{locale} confirmation");
            if locale != "en" {
                assert_ne!(warning, english_warning, "{locale} warning fallback");
                assert_ne!(
                    confirmation, english_confirmation,
                    "{locale} confirmation fallback"
                );
            }
        }
    }
}
