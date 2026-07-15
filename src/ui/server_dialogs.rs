//! Server persistence (`servers.json`) and add-server / auth dialogs.
//!
//! Manages manually-added remote servers (Subsonic, Jellyfin, Plex)
//! that appear in the sidebar.

use adw::prelude::*;
use gtk::glib;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

use crate::local::engine::LibraryEvent;

use super::objects::SourceObject;

type RemoteConnectResult = Result<
    Option<(
        Vec<crate::architecture::models::Track>,
        crate::source_registry::RetainedSource,
    )>,
    (crate::architecture::error::BackendError, bool),
>;

/// Validate a standard remote backend URL before it reaches persistence,
/// logs, an auth dialog, or connection ownership state.
///
/// The error is deliberately fixed text: URL parser diagnostics are not
/// allowed to echo a rejected input that may itself contain credentials.
pub(super) fn validate_remote_server_url(server_url: &str) -> Result<(), &'static str> {
    crate::http_security::parse_base_url(server_url).map(|_| ())
}

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
    let mut servers: Vec<SavedServer> = servers_json_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = servers.len();
    servers.retain(|server| validate_remote_server_url(&server.url).is_ok());
    if servers.len() != before {
        // Remove legacy unsafe rows from disk without ever formatting their
        // values into a diagnostic.
        save_servers(&servers);
        tracing::warn!(
            removed = before - servers.len(),
            "Removed invalid saved server entries"
        );
    }
    servers
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

fn add_saved_server_to(
    servers: &mut Vec<SavedServer>,
    server_type: &str,
    name: &str,
    url: &str,
) -> Result<bool, &'static str> {
    validate_remote_server_url(url)?;
    if servers.iter().any(|server| server.url == url) {
        return Ok(false);
    }

    servers.push(SavedServer {
        server_type: server_type.to_string(),
        name: name.to_string(),
        url: url.to_string(),
    });
    Ok(true)
}

/// Add a validated server to `servers.json` (dedup by URL).
pub fn add_saved_server(server_type: &str, name: &str, url: &str) -> Result<bool, &'static str> {
    let mut servers = load_saved_servers();
    let added = add_saved_server_to(&mut servers, server_type, name, url)?;
    if added {
        save_servers(&servers);
        info!("Server added to servers.json");
    }
    Ok(added)
}

/// Remove a server from `servers.json` by URL.
pub fn remove_saved_server(url: &str) {
    let mut servers = load_saved_servers();
    let before = servers.len();
    servers.retain(|s| s.url != url);
    if servers.len() != before {
        save_servers(&servers);
        info!("Server removed from servers.json");
    }
}

// ── Auth dialog ─────────────────────────────────────────────────────

/// Present an `adw::AlertDialog` asking for credentials.
///
/// When `password_only` is `true` (DAAP), only a password field is shown
/// and empty passwords are allowed (open shares).
///
/// `on_connect` is called with `(username, password)` if the user
/// clicks Connect. `on_cancel` is called if the user dismisses the
/// dialog (Cancel / Escape / clicked off) so the caller can clear any
/// pending-connection state it set up before showing the dialog.
pub fn show_auth_dialog(
    window: &adw::ApplicationWindow,
    server_name: &str,
    server_url: &str,
    password_only: bool,
    on_connect: impl Fn(String, String) + 'static,
    on_cancel: impl FnOnce() + 'static,
) {
    let body = if password_only {
        format!("{server_url}\n{}", rust_i18n::t!("dialogs.enter_password"))
    } else {
        server_url.to_string()
    };

    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("dialogs.connect_to", name = server_name).as_ref())
        .body(&body)
        .close_response("cancel")
        .default_response("connect")
        .build();

    dialog.add_response("cancel", rust_i18n::t!("dialogs.cancel").as_ref());
    dialog.add_response("connect", rust_i18n::t!("dialogs.connect").as_ref());
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);

    // ── Credential entry fields ─────────────────────────────────────
    let user_entry = gtk::Entry::builder()
        .placeholder_text(rust_i18n::t!("dialogs.username").as_ref())
        .activates_default(true)
        .visible(!password_only)
        .build();

    let pass_entry = gtk::PasswordEntry::builder()
        .placeholder_text(rust_i18n::t!("dialogs.password").as_ref())
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
    let on_cancel_cell = std::rc::Rc::new(std::cell::RefCell::new(Some(on_cancel)));

    dialog.connect_response(None, move |_dialog, response| {
        if response == "connect" {
            let submitted = if password_only {
                // DAAP: password only, allow empty (open shares).
                let pass = pass_entry_clone.text().to_string();
                on_connect(String::new(), pass);
                true
            } else {
                let user = user_entry_clone.text().to_string();
                let pass = pass_entry_clone.text().to_string();
                if !user.is_empty() && !pass.is_empty() {
                    on_connect(user, pass);
                    true
                } else {
                    // User clicked Connect with empty fields — treat as
                    // a cancel so the pending-connection guard clears.
                    false
                }
            };
            if !submitted {
                if let Some(cb) = on_cancel_cell.borrow_mut().take() {
                    cb();
                }
            }
        } else if let Some(cb) = on_cancel_cell.borrow_mut().take() {
            // Cancel / Escape / dismissed.
            cb();
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
        .heading(rust_i18n::t!("dialogs.add_server_heading").as_ref())
        .body(rust_i18n::t!("dialogs.add_server_body").as_ref())
        .close_response("cancel")
        .default_response("connect")
        .build();

    dialog.add_response("cancel", rust_i18n::t!("dialogs.cancel").as_ref());
    dialog.add_response("connect", rust_i18n::t!("dialogs.connect").as_ref());
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);

    // ── Server type dropdown ─────────────────────────────────────────
    // Backend names are proper nouns; left untranslated by design.
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
        .placeholder_text(rust_i18n::t!("dialogs.username").as_ref())
        .activates_default(true)
        .build();

    let pass_entry = gtk::PasswordEntry::builder()
        .placeholder_text(rust_i18n::t!("dialogs.password").as_ref())
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

        // Validate before persistence, sidebar publication, connection state,
        // or URL-bearing logs. Rejected text may itself contain a credential,
        // so only the fixed validation message may cross this boundary.
        if let Err(message) = add_saved_server(backend_type, &display_name, &url) {
            tracing::warn!(error = message, "Manual server URL rejected");
            let _ = engine_tx.try_send(LibraryEvent::Error(message.to_string()));
            return;
        }

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
            let Some(attempt) = crate::source_registry::begin_connect(server_url.clone()) else {
                tracing::debug!("Skipping manual remote connect during shutdown");
                return;
            };
            let result: RemoteConnectResult = match backend_type.as_str() {
                "jellyfin" => {
                    info!("Authenticating with Jellyfin (manual)...");
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
                                Ok(backend) => {
                                    let tracks = backend.all_tracks().await;
                                    Ok(attempt
                                        .retain(Arc::new(backend))
                                        .filter(crate::source_registry::RetainedSource::is_current)
                                        .map(|source| (tracks, source)))
                                }
                                Err(e) => Err((e, attempt.is_latest())),
                            }
                        }
                        Err(e) => Err((e, attempt.is_latest())),
                    }
                }
                "plex" => {
                    info!("Authenticating with Plex (manual)...");
                    match crate::plex::client::PlexClient::authenticate(&server_url, &user, &pass)
                        .await
                    {
                        Ok(client) => {
                            match crate::plex::PlexBackend::from_client(&server_name, client).await
                            {
                                Ok(backend) => {
                                    let tracks = backend.all_tracks().await;
                                    Ok(attempt
                                        .retain(Arc::new(backend))
                                        .filter(crate::source_registry::RetainedSource::is_current)
                                        .map(|source| (tracks, source)))
                                }
                                Err(e) => Err((e, attempt.is_latest())),
                            }
                        }
                        Err(e) => Err((e, attempt.is_latest())),
                    }
                }
                _ => {
                    info!("Authenticating with Subsonic (manual)...");
                    match crate::subsonic::SubsonicBackend::connect(
                        &server_name,
                        &server_url,
                        &user,
                        &pass,
                    )
                    .await
                    {
                        Ok(backend) => {
                            let tracks = backend.all_tracks().await;
                            Ok(attempt
                                .retain(Arc::new(backend))
                                .filter(crate::source_registry::RetainedSource::is_current)
                                .map(|source| (tracks, source)))
                        }
                        Err(e) => Err((e, attempt.is_latest())),
                    }
                }
            };

            match result {
                Ok(Some((tracks, source))) => {
                    info!(
                        backend = %backend_type,
                        count = tracks.len(),
                        "Manual server library fetched"
                    );
                    let _ = engine_tx
                        .send(LibraryEvent::RemoteSync {
                            source_key: server_url,
                            generation: source.generation(),
                            lease_key: source.lease_key(),
                            tracks,
                        })
                        .await;
                }
                Ok(None) => tracing::debug!(
                    backend = %backend_type,
                    "Manual remote connection was superseded"
                ),
                Err((_e, false)) => tracing::debug!(
                    backend = %backend_type,
                    "Ignoring superseded manual remote connection failure"
                ),
                Err((e, true)) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_server_urls_never_enter_the_persistence_snapshot() {
        let secret = uuid::Uuid::new_v4().to_string();
        let mut servers = vec![SavedServer {
            server_type: "subsonic".to_string(),
            name: "Existing".to_string(),
            url: "https://existing.example.test".to_string(),
        }];
        let before = serde_json::to_string(&servers).expect("serialize original snapshot");

        for rejected in [
            format!("https://user:{secret}@music.example.test"),
            format!("https://music.example.test?api_key={secret}"),
            format!("https://music.example.test#{secret}"),
        ] {
            let result = add_saved_server_to(&mut servers, "subsonic", "Rejected", &rejected);
            assert!(result.is_err());
            assert_eq!(
                serde_json::to_string(&servers).expect("serialize unchanged snapshot"),
                before
            );
        }

        assert!(!before.contains(&secret));
    }

    #[test]
    fn server_url_validation_errors_never_echo_rejected_secrets() {
        let secret = uuid::Uuid::new_v4().to_string();
        let mut fixed_error = None;
        for rejected in [
            format!("https://user:{secret}@music.example.test"),
            format!("https://music.example.test?token={secret}"),
            format!("not a URL containing {secret}"),
        ] {
            let error = validate_remote_server_url(&rejected)
                .expect_err("credential-bearing or malformed URL must fail");
            if let Some(expected) = fixed_error {
                assert_eq!(error, expected);
            } else {
                fixed_error = Some(error);
            }
            assert!(!error.contains(&secret));
        }
    }
}
