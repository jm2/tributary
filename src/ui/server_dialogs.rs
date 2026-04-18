//! Server persistence (`servers.json`) and add-server / auth dialogs.
//!
//! Manages manually-added remote servers (Subsonic, Jellyfin, Plex)
//! that appear in the sidebar.

use adw::prelude::*;
use gtk::glib;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::local::engine::LibraryEvent;

use super::objects::SourceObject;

// ── SavedServer persistence ─────────────────────────────────────────

/// A saved server entry in `servers.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedServer {
    /// Backend type: `"subsonic"`, `"jellyfin"`, or `"plex"`.
    #[serde(rename = "type")]
    pub server_type: String,
    /// Human-readable display name.
    pub name: String,
    /// Server URL.
    pub url: String,
}

/// Path to `servers.json`: `<data_dir>/tributary/servers.json`.
fn servers_json_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("servers.json"))
}

/// Load saved servers from `servers.json`, returning an empty vec on error.
pub fn load_saved_servers() -> Vec<SavedServer> {
    servers_json_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save the list of servers to `servers.json`.
fn save_servers(servers: &[SavedServer]) {
    if let Some(path) = servers_json_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(servers) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Add a server to `servers.json` (dedup by URL).
pub fn add_saved_server(server_type: &str, name: &str, url: &str) {
    let mut servers = load_saved_servers();
    if !servers.iter().any(|s| s.url == url) {
        servers.push(SavedServer {
            server_type: server_type.to_string(),
            name: name.to_string(),
            url: url.to_string(),
        });
        save_servers(&servers);
        info!(url = %url, "Server added to servers.json");
    }
}

/// Remove a server from `servers.json` by URL.
pub fn remove_saved_server(url: &str) {
    let mut servers = load_saved_servers();
    let before = servers.len();
    servers.retain(|s| s.url != url);
    if servers.len() != before {
        save_servers(&servers);
        info!(url = %url, "Server removed from servers.json");
    }
}

// ── Auth dialog ─────────────────────────────────────────────────────

/// Present an `adw::AlertDialog` asking for credentials.
///
/// When `password_only` is `true` (DAAP), only a password field is shown
/// and empty passwords are allowed (open shares).
///
/// `on_connect` is called with `(username, password)` if the user
/// clicks Connect.  Cancel / Escape simply dismisses the dialog.
pub fn show_auth_dialog(
    window: &adw::ApplicationWindow,
    server_name: &str,
    server_url: &str,
    password_only: bool,
    on_connect: impl Fn(String, String) + 'static,
) {
    let body = if password_only {
        format!("{server_url}\nEnter the share password (leave blank if none)")
    } else {
        server_url.to_string()
    };

    let dialog = adw::AlertDialog::builder()
        .heading(format!("Connect to {server_name}"))
        .body(&body)
        .close_response("cancel")
        .default_response("connect")
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("connect", "Connect");
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);

    // ── Credential entry fields ─────────────────────────────────────
    let user_entry = gtk::Entry::builder()
        .placeholder_text("Username")
        .activates_default(true)
        .visible(!password_only)
        .build();

    let pass_entry = gtk::PasswordEntry::builder()
        .placeholder_text("Password")
        .show_peek_icon(true)
        .activates_default(true)
        .build();

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .build();
    vbox.append(&user_entry);
    vbox.append(&pass_entry);

    dialog.set_extra_child(Some(&vbox));

    let user_entry_clone = user_entry.clone();
    let pass_entry_clone = pass_entry.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response == "connect" {
            if password_only {
                // DAAP: password only, allow empty (open shares).
                let pass = pass_entry_clone.text().to_string();
                on_connect(String::new(), pass);
            } else {
                let user = user_entry_clone.text().to_string();
                let pass = pass_entry_clone.text().to_string();
                if !user.is_empty() && !pass.is_empty() {
                    on_connect(user, pass);
                }
            }
        }
    });

    dialog.present(Some(window));
}

// ── Add Server dialog ───────────────────────────────────────────────

/// Present the "Add Server" dialog.
///
/// Server type dropdown: Subsonic, Jellyfin, Plex (no DAAP).
/// Fields: URL, Username, Password.
/// On "Connect": adds to sidebar + servers.json, then triggers auth.
pub fn show_add_server_dialog(
    window: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    engine_tx: &async_channel::Sender<LibraryEvent>,
    rt_handle: &tokio::runtime::Handle,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Add Server")
        .body("Enter the server details to connect.")
        .close_response("cancel")
        .default_response("connect")
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("connect", "Connect");
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);

    // ── Server type dropdown ─────────────────────────────────────────
    let type_model = gtk::StringList::new(&["Subsonic", "Jellyfin", "Plex"]);
    let type_dropdown = gtk::DropDown::builder()
        .model(&type_model)
        .selected(0)
        .build();

    let url_entry = gtk::Entry::builder()
        .placeholder_text("https://music.example.com")
        .activates_default(true)
        .build();

    let user_entry = gtk::Entry::builder()
        .placeholder_text("Username")
        .activates_default(true)
        .build();

    let pass_entry = gtk::PasswordEntry::builder()
        .placeholder_text("Password")
        .show_peek_icon(true)
        .activates_default(true)
        .build();

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .build();
    vbox.append(&type_dropdown);
    vbox.append(&url_entry);
    vbox.append(&user_entry);
    vbox.append(&pass_entry);

    dialog.set_extra_child(Some(&vbox));

    let store = sidebar_store.clone();
    let engine_tx = engine_tx.clone();
    let rt_handle = rt_handle.clone();

    let url_entry_c = url_entry.clone();
    let user_entry_c = user_entry.clone();
    let pass_entry_c = pass_entry.clone();
    let type_dropdown_c = type_dropdown.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response != "connect" {
            return;
        }

        let url = url_entry_c.text().to_string().trim().to_string();
        let user = user_entry_c.text().to_string();
        let pass = pass_entry_c.text().to_string();

        if url.is_empty() || user.is_empty() || pass.is_empty() {
            return;
        }

        let backend_type = match type_dropdown_c.selected() {
            1 => "jellyfin",
            2 => "plex",
            _ => "subsonic",
        };

        // Derive a display name from the URL host.
        let display_name = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_else(|| url.clone());

        // Persist to servers.json (type, name, url — no credentials).
        add_saved_server(backend_type, &display_name, &url);

        // Add to sidebar as a manual server.
        let insert_pos = super::window::ensure_category_header_store(&store, backend_type);
        let src = SourceObject::manual(&display_name, backend_type, &url);
        src.set_connecting(true);
        store.insert(insert_pos, &src);

        // One-shot to clear spinner on failure.
        let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
        let store_for_fail = store.clone();
        let url_for_fail = url.clone();
        glib::MainContext::default().spawn_local(async move {
            if fail_rx.recv().await.is_ok() {
                for i in 0..store_for_fail.n_items() {
                    if let Some(src) = store_for_fail.item(i).and_downcast_ref::<SourceObject>() {
                        if src.server_url() == url_for_fail {
                            src.set_connecting(false);
                            let src = src.clone();
                            store_for_fail.remove(i);
                            store_for_fail.insert(i, &src);
                            break;
                        }
                    }
                }
            }
        });

        // Spawn auth + fetch on tokio.
        let engine_tx = engine_tx.clone();
        let server_url = url.clone();
        let server_name = display_name.clone();
        let backend_type = backend_type.to_string();

        rt_handle.spawn(async move {
            let result: Result<
                Vec<crate::architecture::models::Track>,
                crate::architecture::error::BackendError,
            > = match backend_type.as_str() {
                "jellyfin" => {
                    info!(server = %server_url, "Authenticating with Jellyfin (manual)...");
                    match crate::jellyfin::client::JellyfinClient::authenticate(
                        &server_url,
                        &user,
                        &pass,
                    )
                    .await
                    {
                        Ok(client) => {
                            match crate::jellyfin::JellyfinBackend::from_client(
                                &server_name,
                                client,
                            )
                            .await
                            {
                                Ok(backend) => Ok(backend.all_tracks().await),
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                "plex" => {
                    info!(server = %server_url, "Authenticating with Plex (manual)...");
                    match crate::plex::client::PlexClient::authenticate(&server_url, &user, &pass)
                        .await
                    {
                        Ok(client) => {
                            match crate::plex::PlexBackend::from_client(&server_name, client).await
                            {
                                Ok(backend) => Ok(backend.all_tracks().await),
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                _ => {
                    info!(server = %server_url, "Authenticating with Subsonic (manual)...");
                    match crate::subsonic::SubsonicBackend::connect(
                        &server_name,
                        &server_url,
                        &user,
                        &pass,
                    )
                    .await
                    {
                        Ok(backend) => Ok(backend.all_tracks().await),
                        Err(e) => Err(e),
                    }
                }
            };

            match result {
                Ok(tracks) => {
                    info!(
                        backend = %backend_type,
                        count = tracks.len(),
                        "Manual server library fetched"
                    );
                    let _ = engine_tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: server_url,
                            tracks,
                        })
                        .await;
                }
                Err(e) => {
                    tracing::error!(
                        backend = %backend_type,
                        error = %e,
                        "Manual server auth failed"
                    );
                    let _ = engine_tx
                        .send(LibraryEvent::Error(format!(
                            "{backend_type} auth failed: {e}"
                        )))
                        .await;
                    let _ = fail_tx.send(()).await;
                }
            }
        });
    });

    dialog.present(Some(window));
}
