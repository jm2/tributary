//! Audio output persistence (`outputs.json`) and the "Add Output" dialog.
//!
//! Manages saved MPD outputs that appear in the header bar output
//! selector popover.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedOutputUpsert {
    Added,
    Upgraded,
    Unchanged,
}

/// Path to `outputs.json`: `<data_dir>/tributary/outputs.json`.
fn outputs_json_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("outputs.json"))
}

/// Load saved outputs from `outputs.json`, returning an empty vec on error.
pub fn load_saved_outputs() -> Vec<SavedOutput> {
    outputs_json_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save the list of outputs to `outputs.json`.
fn save_outputs(outputs: &[SavedOutput]) {
    if let Some(path) = outputs_json_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(outputs) {
            let _ = std::fs::write(path, json);
        }
    }
}

fn upsert_saved_output(
    outputs: &mut Vec<SavedOutput>,
    output_type: &str,
    name: &str,
    host: &str,
    port: u16,
    exclusive_control: bool,
) -> SavedOutputUpsert {
    if let Some(existing) = outputs
        .iter_mut()
        .find(|output| output.host == host && output.port == port)
    {
        // Re-adding a legacy endpoint is the explicit migration path. Preserve
        // its existing type and display name so the already-rendered selector
        // row remains an exact representation of the saved entry.
        if exclusive_control && !existing.exclusive_control {
            existing.exclusive_control = true;
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
    });
    SavedOutputUpsert::Added
}

/// Add or explicitly upgrade an output in `outputs.json` (dedup by host:port).
pub fn add_saved_output(
    output_type: &str,
    name: &str,
    host: &str,
    port: u16,
    exclusive_control: bool,
) -> SavedOutputUpsert {
    let mut outputs = load_saved_outputs();
    let outcome = upsert_saved_output(
        &mut outputs,
        output_type,
        name,
        host,
        port,
        exclusive_control,
    );
    if !matches!(outcome, SavedOutputUpsert::Unchanged) {
        save_outputs(&outputs);
        info!(host = %host, port, ?outcome, "Output saved to outputs.json");
    }
    outcome
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

/// Remove an output from `outputs.json` by host:port.
#[allow(dead_code)]
pub fn remove_saved_output(host: &str, port: u16) {
    let mut outputs = load_saved_outputs();
    let key = format!("{host}:{port}");
    let before = outputs.len();
    outputs.retain(|o| format!("{}:{}", o.host, o.port) != key);
    if outputs.len() != before {
        save_outputs(&outputs);
        info!(host = %host, port, "Output removed from outputs.json");
    }
}

/// Present the "Add Output" dialog.
///
/// Currently supports MPD outputs only.  The dialog collects a display
/// name, host, and port, then probes the MPD server on a background
/// thread to validate connectivity before saving.
pub fn show_add_output_dialog(window: &adw::ApplicationWindow, output_list: &gtk::ListBox) {
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

    dialog.set_extra_child(Some(&grid));
    dialog.set_response_enabled("add", false);
    let dialog_for_confirmation = dialog.clone();
    exclusive_confirmation.connect_toggled(move |confirmation| {
        dialog_for_confirmation.set_response_enabled("add", confirmation.is_active());
    });

    let output_list = output_list.clone();
    let name_entry_c = name_entry.clone();
    let host_entry_c = host_entry.clone();
    let port_spin_c = port_spin.clone();
    let exclusive_confirmation_c = exclusive_confirmation.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response != "add" || !exclusive_confirmation_c.is_active() {
            return;
        }

        let name = name_entry_c.text().to_string().trim().to_string();
        let host = host_entry_c.text().to_string().trim().to_string();
        let port = port_spin_c.value() as u16;

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
        let name = name.clone();
        let host = host.clone();
        glib::MainContext::default().spawn_local(async move {
            if let Ok(result) = probe_rx.recv().await {
                match result {
                    Ok(version) => {
                        info!(
                            name = %name,
                            host = %host,
                            port,
                            version = %version,
                            "MPD output added successfully"
                        );
                        let outcome = add_saved_output("mpd", &name, &host, port, true);

                        // A legacy endpoint is upgraded in place; its row is
                        // already present and retains its saved display name.
                        if outcome == SavedOutputUpsert::Added {
                            let row = super::header_bar::build_output_row(
                                &name,
                                "network-server-symbolic",
                                false,
                            );
                            output_list.append(&row);
                        }
                    }
                    Err(e) => {
                        warn!(
                            host = %host,
                            port,
                            error = %e,
                            "MPD probe failed — output not added"
                        );
                    }
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

        assert_eq!(
            upsert_saved_output(&mut outputs, "mpd", "Replacement", "mpd.local", 6600, true),
            SavedOutputUpsert::Upgraded
        );
        let serialized = serde_json::to_string(&outputs).expect("approved JSON");
        assert!(serialized.contains(r#""exclusive_control":true"#));
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
            },
            SavedOutput {
                output_type: "mpd".to_string(),
                name: "Office".to_string(),
                host: "office.local".to_string(),
                port: 6601,
                exclusive_control: true,
            },
        ];
        let sibling = outputs[1].clone();

        assert_eq!(
            upsert_saved_output(&mut outputs, "mpd", "Renamed", "mpd.local", 6600, true),
            SavedOutputUpsert::Upgraded
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "Living Room");
        assert!(outputs[0].exclusive_control);
        assert_eq!(outputs[1], sibling);
        assert_eq!(
            upsert_saved_output(&mut outputs, "mpd", "Renamed", "mpd.local", 6600, true),
            SavedOutputUpsert::Unchanged
        );
        assert_eq!(outputs.len(), 2);
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
}
