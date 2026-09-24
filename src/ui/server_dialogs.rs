//! Server persistence (`servers.json`) and add-server / auth dialogs.
//!
//! Manages manually-added remote servers (Subsonic, Jellyfin, Plex)
//! that appear in the sidebar.

use adw::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::{info, warn};

use crate::architecture::SourceId;
use crate::local::engine::LibraryEvent;

use super::objects::SourceObject;

// A saved server connects through the URL the user entered. Unauthenticated
// discovery addresses never apply to it, so these helpers take no route.

async fn authenticate_manual_jellyfin(
    server_url: &str,
    username: &str,
    password: &str,
) -> crate::architecture::backend::BackendResult<crate::jellyfin::client::JellyfinClient> {
    crate::jellyfin::client::JellyfinClient::authenticate_with_route(
        server_url, username, password, None,
    )
    .await
}

async fn authenticate_manual_plex(
    server_url: &str,
    username: &str,
    password: &str,
) -> crate::architecture::backend::BackendResult<crate::plex::client::PlexClient> {
    crate::plex::client::PlexClient::authenticate(server_url, username, password).await
}

async fn connect_manual_subsonic(
    server_name: &str,
    server_url: &str,
    username: &str,
    password: &str,
) -> crate::architecture::backend::BackendResult<crate::subsonic::SubsonicBackend> {
    crate::subsonic::SubsonicBackend::connect(server_name, server_url, username, password).await
}

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
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedServer {
    /// Backend type: `"subsonic"`, `"jellyfin"`, `"plex"`, or `"daap"`.
    #[serde(rename = "type")]
    pub server_type: String,
    /// Human-readable display name.
    pub name: String,
    /// Server URL.
    pub url: String,
    /// Stable source identity, independent of endpoint spelling or rebinding.
    pub source_id: SourceId,
}

const SAVED_SERVER_SCHEMA_VERSION: u32 = 1;
const SAVED_SERVER_CONFIG_UNAVAILABLE: &str = "Saved server configuration is unavailable";

#[derive(Debug, Clone, Deserialize)]
struct LegacySavedServer {
    #[serde(rename = "type")]
    server_type: String,
    name: String,
    url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedServerEnvelope {
    schema_version: u32,
    servers: Vec<SavedServer>,
}

enum SavedServerLoad {
    Ready(Vec<SavedServer>),
    Quarantined,
}

/// Path to `servers.json`: `<data_dir>/tributary/servers.json`.
fn servers_json_path() -> Option<std::path::PathBuf> {
    crate::paths::data_dir().map(|d| d.join("tributary").join("servers.json"))
}

/// Load, validate, and (when needed) migrate saved servers.
///
/// Invalid or unavailable configuration is quarantined in place and publishes
/// no rows into the live source registry.
pub fn load_saved_servers() -> Vec<SavedServer> {
    let Some(path) = servers_json_path() else {
        warn!("Saved server configuration path is unavailable");
        return Vec::new();
    };
    match load_saved_servers_from(&path) {
        SavedServerLoad::Ready(servers) => servers,
        SavedServerLoad::Quarantined => {
            warn!("Saved server configuration was quarantined");
            Vec::new()
        }
    }
}

fn load_saved_servers_from(path: &Path) -> SavedServerLoad {
    load_saved_servers_from_with(path, save_servers_to)
}

fn load_saved_servers_from_with(
    path: &Path,
    save_servers: impl FnOnce(&Path, &[SavedServer]) -> std::io::Result<()>,
) -> SavedServerLoad {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SavedServerLoad::Ready(Vec::new())
        }
        Err(_) => return SavedServerLoad::Quarantined,
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return SavedServerLoad::Quarantined,
    };

    if value.is_array() {
        let legacy: Vec<LegacySavedServer> = match serde_json::from_value(value) {
            Ok(legacy) => legacy,
            Err(_) => return SavedServerLoad::Quarantined,
        };
        let (servers, removed) = migrate_legacy_servers(legacy);
        if save_servers(path, &servers).is_err() {
            return SavedServerLoad::Quarantined;
        }
        if removed > 0 {
            warn!(
                removed,
                "Removed invalid or duplicate legacy saved server entries"
            );
        }
        return SavedServerLoad::Ready(servers);
    }

    let envelope: SavedServerEnvelope = match serde_json::from_value::<SavedServerEnvelope>(value) {
        Ok(envelope) if envelope.schema_version == SAVED_SERVER_SCHEMA_VERSION => envelope,
        Ok(_) | Err(_) => return SavedServerLoad::Quarantined,
    };
    validate_version_one_servers(envelope.servers)
        .map(SavedServerLoad::Ready)
        .unwrap_or(SavedServerLoad::Quarantined)
}

fn migrate_legacy_servers(legacy: Vec<LegacySavedServer>) -> (Vec<SavedServer>, usize) {
    let original_len = legacy.len();
    let mut seen = HashSet::new();
    let mut servers = Vec::with_capacity(original_len);
    for row in legacy {
        let Some((url, endpoint_key)) = validated_endpoint(&row.server_type, &row.url) else {
            continue;
        };
        let key = (row.server_type.clone(), endpoint_key);
        if !seen.insert(key) {
            continue;
        }
        let source_id = SourceId::remote(&row.server_type, &url)
            .expect("validated backend and remote URL produce a source ID");
        servers.push(SavedServer {
            server_type: row.server_type,
            name: row.name,
            url: row.url,
            source_id,
        });
    }
    let removed = original_len.saturating_sub(servers.len());
    (servers, removed)
}

fn validate_version_one_servers(servers: Vec<SavedServer>) -> Option<Vec<SavedServer>> {
    let mut by_endpoint: HashMap<(String, String), SourceId> = HashMap::new();
    let mut by_id: HashMap<SourceId, (String, String)> = HashMap::new();
    let mut accepted = Vec::with_capacity(servers.len());

    for server in servers {
        let (base_url, canonical) = validated_endpoint(&server.server_type, &server.url)?;
        if !server
            .source_id
            .is_valid_persisted_remote_for(&server.server_type, &base_url)
        {
            return None;
        }
        let endpoint = (server.server_type.clone(), canonical);
        if let Some(existing) = by_endpoint.get(&endpoint) {
            if *existing != server.source_id {
                return None;
            }
            // An exact duplicate describes one source. Preserve the first
            // file-order row and its display name without rewriting the file.
            continue;
        }
        if let Some(existing) = by_id.get(&server.source_id) {
            if existing != &endpoint {
                return None;
            }
        }
        by_endpoint.insert(endpoint.clone(), server.source_id);
        by_id.insert(server.source_id, endpoint);
        accepted.push(server);
    }
    Some(accepted)
}

fn validated_endpoint(server_type: &str, raw_url: &str) -> Option<(url::Url, String)> {
    if !matches!(server_type, "subsonic" | "jellyfin" | "plex" | "daap") {
        return None;
    }
    let parsed = crate::http_security::parse_base_url(raw_url).ok()?;
    let canonical = crate::architecture::identity::canonical_remote_base_url(&parsed).ok()?;
    Some((parsed, canonical))
}

/// Whether two validated remote descriptions name the same logical source.
///
/// The backend is part of identity: two protocols may legitimately be
/// advertised at the same origin. Endpoint spelling is not identity, so the
/// comparison uses the shared canonical base-URL policy.
pub(super) fn same_remote_endpoint(
    left_type: &str,
    left_url: &str,
    right_type: &str,
    right_url: &str,
) -> bool {
    if left_type != right_type {
        return false;
    }
    validated_endpoint(left_type, left_url)
        .zip(validated_endpoint(right_type, right_url))
        .is_some_and(|((_, left), (_, right))| left == right)
}

fn source_owns_remote_endpoint(source: &SourceObject, server_type: &str, server_url: &str) -> bool {
    same_remote_endpoint(
        &source.backend_type(),
        &source.server_url(),
        server_type,
        server_url,
    )
}

/// Return the one already-published identity for a canonical endpoint before
/// persistence assigns ownership.
///
/// Promotion must persist this ID rather than minting a replacement: the
/// existing value may already own navigation generations, cached tracks,
/// pending authentication, playback, and a retained backend session. A
/// missing, reserved, or duplicate live owner is not safe to promote.
fn existing_source_id_for_endpoint(
    store: &gtk::gio::ListStore,
    server_type: &str,
    server_url: &str,
) -> Result<Option<SourceId>, &'static str> {
    let (base_url, _) =
        validated_endpoint(server_type, server_url).ok_or(SAVED_SERVER_CONFIG_UNAVAILABLE)?;
    let mut owner = None;
    for index in 0..store.n_items() {
        let Some(source) = store.item(index).and_downcast::<SourceObject>() else {
            continue;
        };
        if !source_owns_remote_endpoint(&source, server_type, server_url) {
            continue;
        }
        if owner.is_some() {
            return Err(SAVED_SERVER_CONFIG_UNAVAILABLE);
        }
        let source_id = source
            .source_id()
            .filter(|source_id| source_id.is_valid_persisted_remote_for(server_type, &base_url))
            .ok_or(SAVED_SERVER_CONFIG_UNAVAILABLE)?;
        owner = Some(source_id);
    }
    Ok(owner)
}

/// Publish a saved source without allowing another sidebar row to own the
/// same canonical `(backend, endpoint)` pair.
///
/// A discovered row is promoted in place so its stable identity and ephemeral
/// advertised route survive. Re-adding an already-saved endpoint updates the
/// same object and therefore leaves deletion keyed by one unambiguous
/// persisted `SourceId`.
fn upsert_saved_source_in_store(
    store: &gtk::gio::ListStore,
    selection: Option<&gtk::SingleSelection>,
    saved: &SavedServer,
) -> Result<SourceObject, &'static str> {
    for index in 0..store.n_items() {
        let Some(source) = store.item(index).and_downcast::<SourceObject>() else {
            continue;
        };
        if !source_owns_remote_endpoint(&source, &saved.server_type, &saved.url) {
            continue;
        }

        if source.source_id() != Some(saved.source_id) {
            return Err(SAVED_SERVER_CONFIG_UNAVAILABLE);
        }
        source.set_name(&saved.name);
        source.set_server_url(&saved.url);
        source.set_manually_added(true);
        // A saved row connects through its configured URL, never through the
        // unauthenticated addresses discovery advertised for it.
        source.set_advertised_route(None);
        source.set_connecting(true);

        // SourceObject fields are deliberately plain GTK-side state rather
        // than GObject properties. Reinsert the same object to refresh the
        // list-item binding without creating a second logical owner.
        if let Some(selection) = selection {
            super::window::rebind_sidebar_source(store, selection, index, &source, true);
        } else {
            // Headless model tests do not initialize a GTK selection model.
            store.remove(index);
            store.insert(index, &source);
        }
        return Ok(source);
    }

    let insert_pos = super::window::ensure_category_header_store(store, &saved.server_type);
    let source = SourceObject::manual(&saved.name, &saved.server_type, &saved.url, saved.source_id);
    source.set_connecting(true);
    store.insert(insert_pos, &source);
    Ok(source)
}

/// Save one complete v1 envelope through a same-directory atomic replacement.
fn save_servers_to(path: &Path, servers: &[SavedServer]) -> std::io::Result<()> {
    let envelope = SavedServerEnvelope {
        schema_version: SAVED_SERVER_SCHEMA_VERSION,
        servers: servers.to_vec(),
    };
    let mut json = serde_json::to_vec_pretty(&envelope).map_err(std::io::Error::other)?;
    json.push(b'\n');
    super::persistence::write_atomic(path, &json)
}

fn add_saved_server_to(
    servers: &mut Vec<SavedServer>,
    server_type: &str,
    name: &str,
    url: &str,
    existing_source_id: Option<SourceId>,
) -> Result<bool, &'static str> {
    validate_remote_server_url(url)?;
    let Some((base_url, canonical)) = validated_endpoint(server_type, url) else {
        return Err("Unsupported remote server type");
    };
    if servers.iter().any(|server| {
        validated_endpoint(&server.server_type, &server.url)
            .is_some_and(|(_, existing)| server.server_type == server_type && existing == canonical)
    }) {
        return Ok(false);
    }

    let source_id = if let Some(source_id) = existing_source_id {
        if !source_id.is_valid_persisted_remote_for(server_type, &base_url)
            || servers.iter().any(|server| server.source_id == source_id)
        {
            return Err(SAVED_SERVER_CONFIG_UNAVAILABLE);
        }
        source_id
    } else {
        loop {
            let candidate = SourceId::random();
            if !servers.iter().any(|server| server.source_id == candidate) {
                break candidate;
            }
        }
    };
    servers.push(SavedServer {
        server_type: server_type.to_string(),
        name: name.to_string(),
        url: url.to_string(),
        source_id,
    });
    Ok(true)
}

/// Add a validated server to `servers.json` (dedup by canonical endpoint).
/// An already-published owner is persisted verbatim instead of being
/// reassigned during discovered/environment-to-saved promotion.
pub fn add_saved_server(
    server_type: &str,
    name: &str,
    url: &str,
    existing_source_id: Option<SourceId>,
) -> Result<SavedServer, &'static str> {
    let Some(path) = servers_json_path() else {
        return Err(SAVED_SERVER_CONFIG_UNAVAILABLE);
    };
    let mut servers = match load_saved_servers_from(&path) {
        SavedServerLoad::Ready(servers) => servers,
        SavedServerLoad::Quarantined => return Err(SAVED_SERVER_CONFIG_UNAVAILABLE),
    };
    let added = add_saved_server_to(&mut servers, server_type, name, url, existing_source_id)?;
    if added && save_servers_to(&path, &servers).is_err() {
        return Err(SAVED_SERVER_CONFIG_UNAVAILABLE);
    }
    if added {
        info!("Server added to servers.json");
    }
    let (_, canonical) =
        validated_endpoint(server_type, url).ok_or("Unsupported remote server type")?;
    servers
        .into_iter()
        .find(|server| {
            server.server_type == server_type
                && validated_endpoint(&server.server_type, &server.url)
                    .is_some_and(|(_, existing)| existing == canonical)
        })
        .ok_or(SAVED_SERVER_CONFIG_UNAVAILABLE)
}

/// Remove a server from `servers.json` by stable source identity.
///
/// `true` means persisted absence was confirmed (either the entry was already
/// absent or its removal committed). Callers release the Saved provenance
/// claim only after this succeeds.
pub fn remove_saved_server(source_id: SourceId) -> bool {
    let Some(path) = servers_json_path() else {
        return false;
    };
    let mut servers = match load_saved_servers_from(&path) {
        SavedServerLoad::Ready(servers) => servers,
        SavedServerLoad::Quarantined => return false,
    };
    let before = servers.len();
    servers.retain(|server| server.source_id != source_id);
    if servers.len() == before {
        return true;
    }
    if save_servers_to(&path, &servers).is_ok() {
        info!("Server removed from servers.json");
        true
    } else {
        warn!("Could not persist saved server removal");
        false
    }
}

// ── Credential requirements ─────────────────────────────────────────

/// Which credentials a backend needs before Connect can be submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPolicy {
    /// DAAP: no username, and an empty password opens an unprotected share.
    PasswordOnly,
    /// Jellyfin: a username; accounts without a password are allowed.
    UsernameRequired,
    /// Subsonic and Plex: a username and a password.
    UsernameAndPassword,
}

impl CredentialPolicy {
    pub fn for_backend(backend_type: &str) -> Self {
        match backend_type {
            "daap" => Self::PasswordOnly,
            "jellyfin" => Self::UsernameRequired,
            _ => Self::UsernameAndPassword,
        }
    }

    /// The first required credential that is still empty.
    fn missing(self, username: &str, password: &str) -> Option<MissingServerInput> {
        let needs_username = self != Self::PasswordOnly;
        let needs_password = self == Self::UsernameAndPassword;
        if needs_username && username.trim().is_empty() {
            Some(MissingServerInput::Username)
        } else if needs_password && password.is_empty() {
            Some(MissingServerInput::Password)
        } else {
            None
        }
    }
}

/// A required server-dialog field that is empty or invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingServerInput {
    ServerUrl,
    Username,
    Password,
}

impl MissingServerInput {
    fn message(self, locale: &str) -> String {
        let key = match self {
            Self::ServerUrl => "dialogs.missing_server_url",
            Self::Username => "dialogs.missing_username",
            Self::Password => "dialogs.missing_password",
        };
        rust_i18n::t!(key, locale = locale).into_owned()
    }
}

/// The first field the Add Server dialog still needs, in form order.
fn missing_add_server_input(
    backend_type: &str,
    url: &str,
    username: &str,
    password: &str,
) -> Option<MissingServerInput> {
    if validate_remote_server_url(url.trim()).is_err() {
        return Some(MissingServerInput::ServerUrl);
    }
    CredentialPolicy::for_backend(backend_type).missing(username, password)
}

/// An inline error label for a server dialog, hidden until needed.
fn server_input_error_label() -> gtk::Label {
    let label = gtk::Label::builder()
        .css_classes(["error"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    label.set_accessible_role(gtk::AccessibleRole::Alert);
    label
}

/// Enable Connect only when nothing required is missing. The message is
/// shown only after the user has edited the form, so an untouched dialog
/// opens without an error.
fn apply_server_input_state(
    dialog: &adw::AlertDialog,
    error_label: &gtk::Label,
    missing: Option<MissingServerInput>,
    edited: bool,
) {
    dialog.set_response_enabled("connect", missing.is_none());
    let message = missing
        .filter(|_| edited)
        .map(|missing| missing.message(&rust_i18n::locale()));
    error_label.set_label(message.as_deref().unwrap_or_default());
    error_label.set_visible(message.is_some());
}

// ── Auth dialog ─────────────────────────────────────────────────────

/// Present an `adw::AlertDialog` asking for credentials.
///
/// `policy` decides which fields are shown and required: DAAP shows only a
/// password and allows it to be empty (open shares); Jellyfin requires only a
/// username. Connect stays disabled, with an inline message once the user
/// edits the form, until every required field is filled.
///
/// `on_connect` is called with `(username, password)` if the user
/// clicks Connect. `on_cancel` is called if the user dismisses the
/// dialog (Cancel / Escape / clicked off) so the caller can clear any
/// pending-connection state it set up before showing the dialog.
pub fn show_auth_dialog(
    window: &adw::ApplicationWindow,
    server_name: &str,
    server_url: &str,
    policy: CredentialPolicy,
    on_connect: impl Fn(String, String) + 'static,
    on_cancel: impl FnOnce() + 'static,
) {
    auth_dialog(server_name, server_url, policy, on_connect, on_cancel).present(Some(window));
}

fn auth_dialog(
    server_name: &str,
    server_url: &str,
    policy: CredentialPolicy,
    on_connect: impl Fn(String, String) + 'static,
    on_cancel: impl FnOnce() + 'static,
) -> adw::AlertDialog {
    let password_only = policy == CredentialPolicy::PasswordOnly;
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
    let error_label = server_input_error_label();
    vbox.append(&user_entry);
    vbox.append(&pass_entry);
    vbox.append(&error_label);

    dialog.set_extra_child(Some(&vbox));

    let refresh = {
        let dialog = dialog.downgrade();
        let user_entry = user_entry.downgrade();
        let pass_entry = pass_entry.downgrade();
        let error_label = error_label.downgrade();
        move |edited: bool| {
            let (Some(dialog), Some(user_entry), Some(pass_entry), Some(error_label)) = (
                dialog.upgrade(),
                user_entry.upgrade(),
                pass_entry.upgrade(),
                error_label.upgrade(),
            ) else {
                return;
            };
            let missing = policy.missing(&user_entry.text(), &pass_entry.text());
            apply_server_input_state(&dialog, &error_label, missing, edited);
        }
    };
    refresh(false);
    let refresh = std::rc::Rc::new(refresh);
    {
        let refresh = refresh.clone();
        user_entry.connect_changed(move |_| refresh(true));
    }
    pass_entry.connect_changed(move |_| refresh(true));

    let user_entry_clone = user_entry.clone();
    let pass_entry_clone = pass_entry.clone();
    let on_cancel_cell = std::rc::Rc::new(std::cell::RefCell::new(Some(on_cancel)));

    dialog.connect_response(None, move |_dialog, response| {
        if response == "connect" {
            let user = if password_only {
                String::new()
            } else {
                user_entry_clone.text().to_string()
            };
            let pass = pass_entry_clone.text().to_string();
            // Connect is disabled while a required field is empty; a
            // submission that still lacks one is treated as a cancel so the
            // pending-connection guard clears.
            if policy.missing(&user, &pass).is_none() {
                on_connect(user, pass);
            } else if let Some(cb) = on_cancel_cell.borrow_mut().take() {
                cb();
            }
        } else if let Some(cb) = on_cancel_cell.borrow_mut().take() {
            // Cancel / Escape / dismissed.
            cb();
        }
    });

    dialog
}

// ── Add Server dialog ───────────────────────────────────────────────

/// The backend chosen in the Add Server type dropdown.
fn add_server_backend(selected: u32) -> &'static str {
    match selected {
        1 => "jellyfin",
        2 => "plex",
        _ => "subsonic",
    }
}

/// The Add Server dialog's input widgets.
struct AddServerForm {
    type_dropdown: gtk::DropDown,
    url_entry: gtk::Entry,
    user_entry: gtk::Entry,
    pass_entry: gtk::PasswordEntry,
}

/// Build the Add Server form into `dialog` and keep its Connect response
/// disabled until the form is complete.
fn add_server_form(dialog: &adw::AlertDialog) -> AddServerForm {
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
    let error_label = server_input_error_label();
    vbox.append(&type_dropdown);
    vbox.append(&url_entry);
    vbox.append(&user_entry);
    vbox.append(&pass_entry);
    vbox.append(&error_label);

    dialog.set_extra_child(Some(&vbox));

    // Connect stays disabled until the form is complete, so incomplete input
    // stays in the open dialog with an inline reason instead of being
    // discarded after the dialog closes.
    let refresh = {
        let dialog = dialog.downgrade();
        let type_dropdown = type_dropdown.downgrade();
        let url_entry = url_entry.downgrade();
        let user_entry = user_entry.downgrade();
        let pass_entry = pass_entry.downgrade();
        let error_label = error_label.downgrade();
        move |edited: bool| {
            let (
                Some(dialog),
                Some(type_dropdown),
                Some(url_entry),
                Some(user_entry),
                Some(pass_entry),
                Some(error_label),
            ) = (
                dialog.upgrade(),
                type_dropdown.upgrade(),
                url_entry.upgrade(),
                user_entry.upgrade(),
                pass_entry.upgrade(),
                error_label.upgrade(),
            )
            else {
                return;
            };
            let missing = missing_add_server_input(
                add_server_backend(type_dropdown.selected()),
                &url_entry.text(),
                &user_entry.text(),
                &pass_entry.text(),
            );
            apply_server_input_state(&dialog, &error_label, missing, edited);
        }
    };
    refresh(false);
    let refresh = std::rc::Rc::new(refresh);
    for entry in [
        url_entry.upcast_ref::<gtk::Editable>(),
        user_entry.upcast_ref(),
        pass_entry.upcast_ref(),
    ] {
        let refresh = refresh.clone();
        entry.connect_changed(move |_| refresh(true));
    }
    type_dropdown.connect_selected_notify(move |_| refresh(true));

    AddServerForm {
        type_dropdown,
        url_entry,
        user_entry,
        pass_entry,
    }
}

/// Present the "Add Server" dialog.
///
/// Server type dropdown: Subsonic, Jellyfin, Plex (no DAAP).
/// Fields: URL, Username, Password (optional for Jellyfin). Connect is
/// enabled only once they are complete.
/// On "Connect": adds to sidebar + servers.json, then triggers auth.
pub fn show_add_server_dialog(
    window: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    sidebar_selection: &gtk::SingleSelection,
    engine_tx: &async_channel::Sender<LibraryEvent>,
    source_registry: &crate::source_registry::SourceRegistry,
    remote_provenance: &crate::source_registry::ProvenanceClaims,
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

    let form = add_server_form(&dialog);

    let store = sidebar_store.clone();
    let selection = sidebar_selection.clone();
    let engine_tx = engine_tx.clone();
    let source_registry = source_registry.clone();
    let remote_provenance = remote_provenance.clone();

    let url_entry_c = form.url_entry;
    let user_entry_c = form.user_entry;
    let pass_entry_c = form.pass_entry;
    let type_dropdown_c = form.type_dropdown;

    dialog.connect_response(None, move |_dialog, response| {
        if response != "connect" {
            return;
        }

        let url = url_entry_c.text().to_string().trim().to_string();
        let user = user_entry_c.text().to_string();
        let pass = pass_entry_c.text().to_string();
        let backend_type = add_server_backend(type_dropdown_c.selected());

        // Connect is disabled until the form is complete.
        if missing_add_server_input(backend_type, &url, &user, &pass).is_some() {
            return;
        }

        // Derive a display name from the URL host.
        let display_name = url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_else(|| url.clone());

        // Validate before persistence, sidebar publication, connection state,
        // or URL-bearing logs. Rejected text may itself contain a credential,
        // so only the fixed validation message may cross this boundary.
        let existing_source_id = match existing_source_id_for_endpoint(&store, backend_type, &url) {
            Ok(source_id) => source_id,
            Err(message) => {
                tracing::warn!(
                    error = message,
                    "Manual server source ownership is unavailable"
                );
                let _ = engine_tx.try_send(LibraryEvent::Error(message.to_string()));
                return;
            }
        };
        let saved = match add_saved_server(backend_type, &display_name, &url, existing_source_id) {
            Ok(saved) => saved,
            Err(message) => {
                tracing::warn!(error = message, "Manual server URL rejected");
                let _ = engine_tx.try_send(LibraryEvent::Error(message.to_string()));
                return;
            }
        };
        if !remote_provenance.ensure(
            &source_registry,
            saved.source_id,
            crate::source_lifecycle::SourceProvenance::Saved,
            "saved-config",
        ) {
            tracing::debug!("Saved server persisted while remote lifecycle was closing");
        }

        // Publish exactly one owner for this logical endpoint. A discovered
        // row's already-published identity was persisted above, so this path
        // does not mutate or attempt to transfer any identity-keyed route,
        // cache, navigation, playback, or registry state.
        let saved_source = match upsert_saved_source_in_store(&store, Some(&selection), &saved) {
            Ok(source) => source,
            Err(message) => {
                tracing::warn!(error = message, "Manual server source ownership changed");
                let _ = engine_tx.try_send(LibraryEvent::Error(message.to_string()));
                return;
            }
        };
        let server_url = saved.url.clone();
        let server_name = saved.name.clone();
        let backend_type = backend_type.to_string();
        let source_id = saved.source_id;
        let source_for_generation = saved_source.clone();
        let store_for_generation = store.clone();
        let selection_for_generation = selection.clone();
        let on_generation = move |generation| {
            source_for_generation.set_connecting_generation(generation);
            for index in 0..store_for_generation.n_items() {
                let Some(source) = store_for_generation
                    .item(index)
                    .and_downcast::<SourceObject>()
                else {
                    continue;
                };
                if source.source_id() == Some(source_id) {
                    super::window::rebind_sidebar_source(
                        &store_for_generation,
                        &selection_for_generation,
                        index,
                        &source_for_generation,
                        true,
                    );
                    return;
                }
            }
            debug_assert!(false, "persisted manual source row remains published");
        };

        let generation = match backend_type.as_str() {
            "jellyfin" => source_registry.connect_jellyfin_session(
                source_id,
                on_generation,
                move || async move {
                    info!("Authenticating with Jellyfin (manual)...");
                    let client = authenticate_manual_jellyfin(&server_url, &user, &pass).await?;
                    Ok(crate::jellyfin::JellyfinBackend::stage_authenticated(
                        &server_name,
                        client,
                    ))
                },
            ),
            "plex" => {
                source_registry.connect_standard(source_id, on_generation, move || async move {
                    info!("Authenticating with Plex (manual)...");
                    let client = authenticate_manual_plex(&server_url, &user, &pass).await?;
                    crate::plex::PlexBackend::from_client(&server_name, client).await
                })
            }
            _ => source_registry.connect_standard(source_id, on_generation, move || async move {
                info!("Authenticating with Subsonic (manual)...");
                connect_manual_subsonic(&server_name, &server_url, &user, &pass).await
            }),
        };

        if generation.is_none() {
            saved_source.set_connecting(false);
            for index in 0..store.n_items() {
                let Some(source) = store.item(index).and_downcast::<SourceObject>() else {
                    continue;
                };
                if source.source_id() == Some(source_id) {
                    super::window::rebind_sidebar_source(
                        &store,
                        &selection,
                        index,
                        &saved_source,
                        true,
                    );
                    break;
                }
            }
            tracing::debug!(
                backend = %backend_type,
                "Skipping manual remote connect during shutdown or after source retirement"
            );
        }
    });

    dialog.present(Some(window));
}

/// GTK-touching contracts folded into the crate's single consolidated
/// GTK-initializing test (browser.rs `gtk_widget_contracts_hold_on_one_session`);
/// see `ui::widget_test_session`. Mirrors the caller's macOS gate so these
/// helpers are never dead code there.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// Every widget of one type in the dialog's extra child, in order.
    fn children<W: IsA<gtk::Widget>>(dialog: &adw::AlertDialog) -> Vec<W> {
        let mut found = Vec::new();
        let mut child = dialog.extra_child().and_then(|extra| extra.first_child());
        while let Some(widget) = child {
            if let Some(widget) = widget.downcast_ref::<W>() {
                found.push(widget.clone());
            }
            child = widget.next_sibling();
        }
        found
    }

    fn error_label(dialog: &adw::AlertDialog) -> gtk::Label {
        children::<gtk::Label>(dialog)
            .into_iter()
            .find(|label| label.has_css_class("error"))
            .expect("inline error label")
    }

    /// Jellyfin connects without a password; Subsonic keeps Connect disabled
    /// and says which field is missing once the user starts typing.
    pub fn connect_waits_for_required_credentials() {
        let submitted = Rc::new(RefCell::new(None));
        let sink = submitted.clone();
        let dialog = auth_dialog(
            "Home",
            "https://jellyfin.example.test",
            CredentialPolicy::UsernameRequired,
            move |user, pass| *sink.borrow_mut() = Some((user, pass)),
            || {},
        );
        let user = children::<gtk::Entry>(&dialog).remove(0);
        assert!(!dialog.is_response_enabled("connect"));
        assert!(
            !error_label(&dialog).is_visible(),
            "no error before editing"
        );
        user.set_text("kid");
        assert!(dialog.is_response_enabled("connect"));
        dialog.emit_by_name::<()>("response", &[&"connect"]);
        assert_eq!(
            submitted.borrow().clone(),
            Some(("kid".to_owned(), String::new()))
        );

        let dialog = auth_dialog(
            "Home",
            "https://subsonic.example.test",
            CredentialPolicy::UsernameAndPassword,
            |_, _| panic!("incomplete credentials must not connect"),
            || {},
        );
        children::<gtk::Entry>(&dialog)[0].set_text("me");
        assert!(!dialog.is_response_enabled("connect"));
        let error = error_label(&dialog);
        assert!(error.is_visible());
        assert_eq!(
            error.label(),
            MissingServerInput::Password.message(&rust_i18n::locale())
        );
        children::<gtk::PasswordEntry>(&dialog)[0].set_text("secret");
        assert!(dialog.is_response_enabled("connect"));
        assert!(!error.is_visible());
    }

    /// Incomplete Add Server input keeps Connect disabled with a reason, and
    /// switching to Jellyfin drops the password requirement.
    pub fn add_server_keeps_incomplete_input_with_a_reason() {
        let dialog = adw::AlertDialog::new(None, None);
        dialog.add_response("connect", "Connect");
        let form = add_server_form(&dialog);
        assert!(!dialog.is_response_enabled("connect"));
        assert!(!error_label(&dialog).is_visible());

        form.url_entry.set_text("music.example.test");
        assert_eq!(
            error_label(&dialog).label(),
            MissingServerInput::ServerUrl.message(&rust_i18n::locale())
        );
        form.url_entry.set_text("https://music.example.test");
        form.user_entry.set_text("me");
        assert!(
            !dialog.is_response_enabled("connect"),
            "Subsonic needs a password"
        );
        assert_eq!(
            error_label(&dialog).label(),
            MissingServerInput::Password.message(&rust_i18n::locale())
        );

        form.type_dropdown.set_selected(1);
        assert!(
            dialog.is_response_enabled("connect"),
            "Jellyfin allows no password"
        );
        assert!(!error_label(&dialog).is_visible());
        assert_eq!(form.pass_entry.text(), "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(path: &Path) -> Vec<SavedServer> {
        match load_saved_servers_from(path) {
            SavedServerLoad::Ready(servers) => servers,
            SavedServerLoad::Quarantined => panic!("fixture was quarantined"),
        }
    }

    #[test]
    fn legacy_array_is_migrated_atomically_before_rows_are_published() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("servers.json");
        let legacy = serde_json::json!([
            {
                "type": "subsonic",
                "name": "First spelling wins",
                "url": "HTTPS://MUSIC.EXAMPLE.TEST:443/base/"
            },
            {
                "type": "subsonic",
                "name": "Duplicate",
                "url": "https://music.example.test/base"
            },
            {
                "type": "jellyfin",
                "name": "Same URL, distinct backend",
                "url": "https://music.example.test/base"
            },
            {
                "type": "unsupported",
                "name": "Dropped",
                "url": "https://ignored.example.test"
            }
        ]);
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&legacy).expect("legacy JSON"),
        )
        .expect("write legacy file");

        let servers = loaded(&path);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "First spelling wins");
        assert_eq!(
            servers[0].source_id.to_string(),
            "71bd3508-1650-530c-8f0e-a06a72c64e3b"
        );
        assert_ne!(servers[0].source_id, servers[1].source_id);

        let migrated: SavedServerEnvelope =
            serde_json::from_slice(&std::fs::read(&path).expect("read migration"))
                .expect("v1 envelope");
        assert_eq!(migrated.schema_version, 1);
        assert_eq!(migrated.servers, servers);
    }

    #[test]
    fn unknown_malformed_and_conflicting_v1_files_are_quarantined_unchanged() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("servers.json");
        let source_a = SourceId::random();
        let source_b = SourceId::random();
        let other_endpoint_owner = SourceId::remote(
            "subsonic",
            &url::Url::parse("https://other.example.test").expect("other endpoint"),
        )
        .expect("other endpoint owner");
        let removable_owner =
            SourceId::removable("device:uuid:01234567-89ab-cdef-0123-456789abcdef")
                .expect("removable owner");
        let fixtures = [
            serde_json::json!({ "schema_version": 99, "servers": [] }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "subsonic",
                    "name": "Missing identity",
                    "url": "https://music.example.test"
                }]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [
                    {
                        "type": "subsonic",
                        "name": "First",
                        "url": "https://music.example.test",
                        "source_id": source_a
                    },
                    {
                        "type": "subsonic",
                        "name": "Conflicting endpoint owner",
                        "url": "HTTPS://MUSIC.EXAMPLE.TEST:443/",
                        "source_id": source_b
                    }
                ]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [
                    {
                        "type": "subsonic",
                        "name": "First",
                        "url": "https://first.example.test",
                        "source_id": source_a
                    },
                    {
                        "type": "plex",
                        "name": "Conflicting ID owner",
                        "url": "https://second.example.test",
                        "source_id": source_a
                    }
                ]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "subsonic",
                    "name": "Cannot impersonate local",
                    "url": "https://music.example.test",
                    "source_id": SourceId::local()
                }]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "jellyfin",
                    "name": "Cannot impersonate radio",
                    "url": "https://video.example.test",
                    "source_id": SourceId::radio_browser()
                }]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "plex",
                    "name": "Nil is reserved",
                    "url": "https://plex.example.test",
                    "source_id": SourceId::from_uuid(uuid::Uuid::nil())
                }]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "subsonic",
                    "name": "Cannot claim another endpoint owner",
                    "url": "https://music.example.test",
                    "source_id": other_endpoint_owner
                }]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "subsonic",
                    "name": "Cannot claim a removable owner",
                    "url": "https://music.example.test",
                    "source_id": removable_owner
                }]
            }),
            serde_json::json!({
                "schema_version": 1,
                "servers": [{
                    "type": "subsonic",
                    "name": "Only random v4 or exact remote v5 is valid",
                    "url": "https://music.example.test",
                    "source_id": SourceId::from_uuid(uuid::Uuid::from_u128(
                        0x0000_0000_0000_1000_8000_0000_0000_0001
                    ))
                }]
            }),
        ];

        for fixture in fixtures {
            let original = serde_json::to_vec_pretty(&fixture).expect("fixture JSON");
            std::fs::write(&path, &original).expect("write fixture");
            assert!(matches!(
                load_saved_servers_from(&path),
                SavedServerLoad::Quarantined
            ));
            assert_eq!(std::fs::read(&path).expect("read fixture"), original);
        }
    }

    #[test]
    fn version_one_accepts_random_or_exact_canonical_remote_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("servers.json");
        let deterministic_url =
            url::Url::parse("https://discovered.example.test/base").expect("endpoint");
        let deterministic =
            SourceId::remote("subsonic", &deterministic_url).expect("deterministic owner");
        let random = SourceId::random();
        let envelope = serde_json::json!({
            "schema_version": 1,
            "servers": [
                {
                    "type": "subsonic",
                    "name": "Promoted discovery",
                    "url": "HTTPS://DISCOVERED.EXAMPLE.TEST:443/base/",
                    "source_id": deterministic
                },
                {
                    "type": "plex",
                    "name": "Manual source",
                    "url": "https://manual.example.test",
                    "source_id": random
                }
            ]
        });
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&envelope).expect("fixture JSON"),
        )
        .expect("write fixture");

        let servers = loaded(&path);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].source_id, deterministic);
        assert_eq!(servers[1].source_id, random);
    }

    #[test]
    fn exact_v1_duplicates_preserve_the_first_row_without_changing_the_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("servers.json");
        let source_id = SourceId::random();
        let envelope = serde_json::json!({
            "schema_version": 1,
            "servers": [
                {
                    "type": "subsonic",
                    "name": "First",
                    "url": "https://music.example.test/base/",
                    "source_id": source_id
                },
                {
                    "type": "subsonic",
                    "name": "Duplicate",
                    "url": "HTTPS://MUSIC.EXAMPLE.TEST:443/base",
                    "source_id": source_id
                }
            ]
        });
        let original = serde_json::to_vec_pretty(&envelope).expect("fixture JSON");
        std::fs::write(&path, &original).expect("write fixture");

        let servers = loaded(&path);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "First");
        assert_eq!(std::fs::read(&path).expect("read fixture"), original);
    }

    #[test]
    fn failed_legacy_replacement_publishes_nothing_and_preserves_original_bytes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("servers.json");
        let original = br#"[{"type":"subsonic","name":"Home","url":"https://music.example.test"}]"#;
        std::fs::write(&path, original).expect("write legacy fixture");

        let result = load_saved_servers_from_with(&path, |_path, _servers| {
            Err(std::io::Error::other("injected atomic replacement failure"))
        });

        assert!(matches!(result, SavedServerLoad::Quarantined));
        assert_eq!(std::fs::read(&path).expect("read legacy fixture"), original);
    }

    #[test]
    fn in_memory_add_deduplicates_canonical_endpoint_and_mints_a_persistable_id() {
        let mut servers = Vec::new();
        assert!(add_saved_server_to(
            &mut servers,
            "subsonic",
            "Home",
            "HTTPS://MUSIC.EXAMPLE.TEST:443/base/",
            None,
        )
        .expect("valid server"));
        assert!(!add_saved_server_to(
            &mut servers,
            "subsonic",
            "Duplicate",
            "https://music.example.test/base",
            None,
        )
        .expect("duplicate server"));
        assert!(add_saved_server_to(
            &mut servers,
            "plex",
            "Other backend",
            "https://music.example.test/base",
            None,
        )
        .expect("distinct backend"));
        assert_eq!(servers.len(), 2);
        assert_ne!(servers[0].source_id, servers[1].source_id);
        assert!(serde_json::to_value(&servers[0])
            .expect("serialize row")
            .get("source_id")
            .is_some());
    }

    #[test]
    fn repeated_manual_add_reuses_one_sidebar_owner_and_persisted_id() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        let saved = SavedServer {
            server_type: "subsonic".to_string(),
            name: "Home".to_string(),
            url: "HTTPS://MUSIC.EXAMPLE.TEST:443/base/".to_string(),
            source_id: SourceId::random(),
        };

        let first = upsert_saved_source_in_store(&store, None, &saved).expect("first owner");
        let equivalent = SavedServer {
            name: "Ignored duplicate name".to_string(),
            url: "https://music.example.test/base".to_string(),
            ..saved.clone()
        };
        let second =
            upsert_saved_source_in_store(&store, None, &equivalent).expect("existing owner");

        let owners: Vec<_> = (0..store.n_items())
            .filter_map(|index| store.item(index).and_downcast::<SourceObject>())
            .filter(|source| {
                source_owns_remote_endpoint(
                    source,
                    &saved.server_type,
                    "https://music.example.test/base",
                )
            })
            .collect();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].source_id(), Some(saved.source_id));
        assert_eq!(first.source_id(), second.source_id());
    }

    #[test]
    fn saving_a_discovered_endpoint_persists_and_keeps_its_published_identity() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        let insert_pos = super::super::window::ensure_category_header_store(&store, "subsonic");
        let discovered = SourceObject::discovered(
            "Discovered name",
            "subsonic",
            "HTTPS://MUSIC.EXAMPLE.TEST:443/base/",
        );
        let discovered_source_id = discovered.source_id().expect("deterministic source ID");
        let origin = url::Url::parse("https://music.example.test/base").expect("origin");
        let route = crate::architecture::AdvertisedHttpRoute::new(
            &origin,
            ["192.0.2.9:443".parse().expect("socket address")],
        )
        .expect("route");
        discovered.set_advertised_route(Some(route));
        store.insert(insert_pos, &discovered);

        let existing_source_id =
            existing_source_id_for_endpoint(&store, "subsonic", "https://music.example.test/base")
                .expect("unambiguous live owner");
        assert_eq!(existing_source_id, Some(discovered_source_id));

        let mut servers = Vec::new();
        assert!(add_saved_server_to(
            &mut servers,
            "subsonic",
            "Saved name",
            "https://music.example.test/base",
            existing_source_id,
        )
        .expect("save discovered owner"));
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("servers.json");
        save_servers_to(&path, &servers).expect("persist saved owner");
        let saved = loaded(&path).pop().expect("saved record");
        assert_eq!(saved.source_id, discovered_source_id);

        let promoted =
            upsert_saved_source_in_store(&store, None, &saved).expect("promote same owner");

        let owner_count = (0..store.n_items())
            .filter_map(|index| store.item(index).and_downcast::<SourceObject>())
            .filter(|source| source_owns_remote_endpoint(source, &saved.server_type, &saved.url))
            .count();
        assert_eq!(owner_count, 1);
        assert_eq!(promoted.source_id(), Some(discovered_source_id));
        assert_eq!(discovered.source_id(), Some(discovered_source_id));
        assert_eq!(
            promoted.advertised_route(),
            None,
            "a saved row never keeps a discovery route"
        );
        assert!(promoted.manually_added());
        assert!(promoted.connecting());
    }

    #[test]
    fn rejected_server_urls_never_enter_the_persistence_snapshot() {
        let secret = uuid::Uuid::new_v4().to_string();
        let mut servers = vec![SavedServer {
            server_type: "subsonic".to_string(),
            name: "Existing".to_string(),
            url: "https://existing.example.test".to_string(),
            source_id: SourceId::random(),
        }];
        let before = serde_json::to_string(&servers).expect("serialize original snapshot");

        for rejected in [
            format!("https://user:{secret}@music.example.test"),
            format!("https://music.example.test?api_key={secret}"),
            format!("https://music.example.test#{secret}"),
        ] {
            let result = add_saved_server_to(&mut servers, "subsonic", "Rejected", &rejected, None);
            assert!(result.is_err());
            assert_eq!(
                serde_json::to_string(&servers).expect("serialize unchanged snapshot"),
                before
            );
        }

        assert!(!before.contains(&secret));
    }

    #[test]
    fn jellyfin_accepts_an_empty_password_but_subsonic_and_plex_do_not() {
        let jellyfin = CredentialPolicy::for_backend("jellyfin");
        assert_eq!(jellyfin.missing("kid", ""), None);
        assert_eq!(
            jellyfin.missing("", "secret"),
            Some(MissingServerInput::Username)
        );
        assert_eq!(
            jellyfin.missing("  ", ""),
            Some(MissingServerInput::Username)
        );

        for backend in ["subsonic", "plex"] {
            let policy = CredentialPolicy::for_backend(backend);
            assert_eq!(policy, CredentialPolicy::UsernameAndPassword);
            assert_eq!(policy.missing("me", ""), Some(MissingServerInput::Password));
            assert_eq!(policy.missing("", ""), Some(MissingServerInput::Username));
            assert_eq!(policy.missing("me", "secret"), None);
        }

        let daap = CredentialPolicy::for_backend("daap");
        assert_eq!(
            daap.missing("", ""),
            None,
            "open DAAP shares need no password"
        );
    }

    #[test]
    fn add_server_reports_the_first_missing_field_in_form_order() {
        let url = "https://music.example.test";
        assert_eq!(
            missing_add_server_input("subsonic", "", "", ""),
            Some(MissingServerInput::ServerUrl)
        );
        assert_eq!(
            missing_add_server_input("subsonic", "music.example.test", "me", "secret"),
            Some(MissingServerInput::ServerUrl)
        );
        assert_eq!(
            missing_add_server_input("plex", url, "", "secret"),
            Some(MissingServerInput::Username)
        );
        assert_eq!(
            missing_add_server_input("subsonic", url, "me", ""),
            Some(MissingServerInput::Password)
        );
        assert_eq!(missing_add_server_input("jellyfin", url, "me", ""), None);
        assert_eq!(
            missing_add_server_input("subsonic", &format!(" {url} "), "me", "secret"),
            None
        );
        assert_eq!(add_server_backend(0), "subsonic");
        assert_eq!(add_server_backend(1), "jellyfin");
        assert_eq!(add_server_backend(2), "plex");
    }

    #[test]
    fn missing_server_input_messages_are_localized_for_every_catalog() {
        for missing in [
            MissingServerInput::ServerUrl,
            MissingServerInput::Username,
            MissingServerInput::Password,
        ] {
            let english = missing.message("en");
            assert!(!english.starts_with("dialogs."), "{missing:?}");
            for locale in rust_i18n::available_locales!() {
                let localized = missing.message(&locale);
                assert!(!localized.starts_with("dialogs."), "{locale}: {missing:?}");
                if locale != "en" {
                    assert_ne!(localized, english, "{locale} must not fall back to English");
                }
            }
        }
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
