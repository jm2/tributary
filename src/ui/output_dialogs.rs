//! Audio output persistence (`outputs.json`) and the "Add Output" dialog.
//!
//! Manages saved MPD outputs that appear in the header bar output
//! selector popover.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// A saved audio output entry in `outputs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Add an output to `outputs.json` (dedup by host:port).
pub fn add_saved_output(output_type: &str, name: &str, host: &str, port: u16) {
    let mut outputs = load_saved_outputs();
    let key = format!("{host}:{port}");
    if !outputs
        .iter()
        .any(|o| format!("{}:{}", o.host, o.port) == key)
    {
        outputs.push(SavedOutput {
            output_type: output_type.to_string(),
            name: name.to_string(),
            host: host.to_string(),
            port,
        });
        save_outputs(&outputs);
        info!(host = %host, port, "Output added to outputs.json");
    }
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

    dialog.set_extra_child(Some(&grid));

    let output_list = output_list.clone();
    let name_entry_c = name_entry.clone();
    let host_entry_c = host_entry.clone();
    let port_spin_c = port_spin.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response != "add" {
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
                        add_saved_output("mpd", &name, &host, port);

                        // Add row to the output selector popover.
                        let row = super::header_bar::build_output_row(
                            &name,
                            "network-server-symbolic",
                            false,
                        );
                        output_list.append(&row);
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
