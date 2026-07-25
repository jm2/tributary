//! Playlist sidebar CRUD actions (create, rename, delete, edit smart rules).
//!
//! Handles [`sidebar::PlaylistAction`] events received from the sidebar
//! context menu and wires them to database operations via the
//! [`PlaylistManager`](crate::local::playlist_manager::PlaylistManager).

use adw::prelude::*;
use gtk::glib;
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use tracing::{debug, error, info, warn};

use super::objects::{PlaylistSidebarKind, SourceObject};
use super::sidebar;
use super::window_state::WindowState;

/// Closed worker result: GTK may publish a mutation only after the database
/// operation reports a successful commit. A dropped worker is also treated as
/// failure by every receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaylistCrudOutcome<T> {
    Committed(T),
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistSidebarPublicationEffect {
    RequestFullSnapshot,
    LeavePublishedSnapshot,
}

fn playlist_sidebar_publication_effect<T>(
    outcome: &PlaylistCrudOutcome<T>,
) -> PlaylistSidebarPublicationEffect {
    match outcome {
        PlaylistCrudOutcome::Committed(_) => PlaylistSidebarPublicationEffect::RequestFullSnapshot,
        PlaylistCrudOutcome::Failed => PlaylistSidebarPublicationEffect::LeavePublishedSnapshot,
    }
}

fn request_playlist_sidebar_refresh(
    refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
) {
    if matches!(
        refresh.request(),
        crate::local::playlist_sidebar::PlaylistSidebarRefreshRequest::Closed
    ) {
        warn!("Playlist sidebar publisher stopped before committed mutation refresh");
    }
}

/// Wire the playlist action receiver to the sidebar store.
///
/// Spawns an async task on the GTK main context that listens for
/// [`sidebar::PlaylistAction`] events and dispatches to the appropriate
/// dialog + DB handler.
pub fn setup_playlist_actions(
    state: &WindowState,
    playlist_action_rx: async_channel::Receiver<sidebar::PlaylistAction>,
    browse_server_playlists: Rc<dyn Fn()>,
    window_closing: Rc<Cell<bool>>,
) {
    let sidebar_store = state.sidebar_store.clone();
    let rt_handle = state.rt_handle.clone();
    let win = state.window.clone();
    let playlist_sidebar_refresh = state.playlist_sidebar_refresh.clone();

    debug!("setup_playlist_actions: async task spawned");
    glib::MainContext::default().spawn_local(async move {
        debug!("setup_playlist_actions: async task started polling");
        while let Ok(action) = playlist_action_rx.recv().await {
            debug!(?action, "setup_playlist_actions: action received, window_closing={}", window_closing.get());

            if window_closing.get() {
                debug!("setup_playlist_actions: window_closing, breaking loop");
                break;
            }
            match action {
                sidebar::PlaylistAction::CreateRegular => {
                    debug!("dispatching CreateRegular");
                    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                        handle_create_regular(&win, &rt_handle, &playlist_sidebar_refresh);
                    })) {
                        error!("CreateRegular handler panicked: {e:?}");
                    }
                }

                sidebar::PlaylistAction::CreateSmart => {
                    debug!("dispatching CreateSmart");
                    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                        handle_create_smart(&win, &rt_handle, &playlist_sidebar_refresh);
                    })) {
                        error!("CreateSmart handler panicked: {e:?}");
                    }
                }

                sidebar::PlaylistAction::Rename(playlist_id) => {
                    debug!(id = %playlist_id, "dispatching Rename");
                    if playlist_allows_ordinary_actions(&sidebar_store, &playlist_id) {
                        if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                            handle_rename(
                                &win,
                                &sidebar_store,
                                &rt_handle,
                                &playlist_sidebar_refresh,
                                &playlist_id,
                            );
                        })) {
                            error!(id = %playlist_id, "Rename handler panicked: {e:?}");
                        }
                    } else {
                        debug!(id = %playlist_id, "Rename: playlist_allows_ordinary_actions rejected");
                    }
                }

                sidebar::PlaylistAction::Delete(playlist_id) => {
                    debug!(id = %playlist_id, "dispatching Delete");
                    if playlist_allows_ordinary_actions(&sidebar_store, &playlist_id) {
                        if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                            handle_delete(
                                &win,
                                &sidebar_store,
                                &rt_handle,
                                &playlist_sidebar_refresh,
                                &playlist_id,
                            );
                        })) {
                            error!(id = %playlist_id, "Delete handler panicked: {e:?}");
                        }
                    } else {
                        debug!(id = %playlist_id, "Delete: playlist_allows_ordinary_actions rejected");
                    }
                }

                sidebar::PlaylistAction::EditSmart(playlist_id) => {
                    debug!(id = %playlist_id, "dispatching EditSmart");
                    if playlist_is_editable_smart(&sidebar_store, &playlist_id) {
                        if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                            handle_edit_smart(
                                &win,
                                &sidebar_store,
                                &rt_handle,
                                &playlist_sidebar_refresh,
                                &playlist_id,
                            );
                        })) {
                            error!(id = %playlist_id, "EditSmart handler panicked: {e:?}");
                        }
                    } else {
                        debug!(id = %playlist_id, "EditSmart: playlist_is_editable_smart rejected");
                    }
                }

                sidebar::PlaylistAction::ImportPlaylist => {
                    debug!("dispatching ImportPlaylist");
                    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                        handle_import_playlist(&win, &rt_handle, &playlist_sidebar_refresh);
                    })) {
                        error!("ImportPlaylist handler panicked: {e:?}");
                    }
                }

                sidebar::PlaylistAction::BrowseServerPlaylists => {
                    debug!("dispatching BrowseServerPlaylists");
                    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                        browse_server_playlists();
                    })) {
                        error!("BrowseServerPlaylists handler panicked: {e:?}");
                    }
                }

                sidebar::PlaylistAction::ExportPlaylist(playlist_id) => {
                    debug!(id = %playlist_id, "dispatching ExportPlaylist");
                    if playlist_allows_ordinary_actions(&sidebar_store, &playlist_id) {
                        if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
                            handle_export_playlist(&win, &sidebar_store, &rt_handle, &playlist_id);
                        })) {
                            error!(id = %playlist_id, "ExportPlaylist handler panicked: {e:?}");
                        }
                    } else {
                        debug!(id = %playlist_id, "ExportPlaylist: playlist_allows_ordinary_actions rejected");
                    }
                }
            }
        }
        info!("setup_playlist_actions: async task finished (channel closed or loop exited)");
    });
}

// ═══════════════════════════════════════════════════════════════════════
// Individual action handlers
// ═══════════════════════════════════════════════════════════════════════

/// Show a name dialog and create a new regular playlist.
fn handle_create_regular(
    win: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    playlist_sidebar_refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
) {
    info!("Creating new regular playlist");
    let rt_handle = rt_handle.clone();
    let win_for_result = win.clone();
    let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();

    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("dialogs.new_playlist_heading").as_ref())
        .close_response("cancel")
        .default_response("create")
        .build();
    dialog.add_response("cancel", rust_i18n::t!("dialogs.cancel").as_ref());
    dialog.add_response("create", rust_i18n::t!("dialogs.create").as_ref());
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);

    let name_entry = gtk::Entry::builder()
        .placeholder_text(rust_i18n::t!("dialogs.playlist_name").as_ref())
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&name_entry));

    dialog.connect_response(None, move |_dialog, response| {
        if response != "create" {
            return;
        }
        let name = name_entry.text().to_string();
        if name.is_empty() {
            return;
        }

        let win = win_for_result.clone();
        let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
        let (result_tx, result_rx) = async_channel::bounded::<PlaylistCrudOutcome<()>>(1);

        rt_handle.spawn(async move {
            let outcome = match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    match mgr.create_regular_playlist(&name).await {
                        Ok(_) => PlaylistCrudOutcome::Committed(()),
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to create playlist");
                            PlaylistCrudOutcome::Failed
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB");
                    PlaylistCrudOutcome::Failed
                }
            };
            let _ = result_tx.send(outcome).await;
        });

        glib::MainContext::default().spawn_local(async move {
            match result_rx.recv().await {
                Ok(outcome)
                    if playlist_sidebar_publication_effect(&outcome)
                        == PlaylistSidebarPublicationEffect::RequestFullSnapshot =>
                {
                    request_playlist_sidebar_refresh(&playlist_sidebar_refresh);
                }
                Ok(_) | Err(_) => show_playlist_mutation_failed(&win),
            }
        });
    });

    dialog.present(Some(win));
}

/// Open the smart playlist editor and create a new smart playlist.
fn handle_create_smart(
    win: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    playlist_sidebar_refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
) {
    info!("Creating new smart playlist");
    let rt_handle = rt_handle.clone();
    let win_for_result = win.clone();
    let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
    let default_name = rust_i18n::t!("smart_playlist.new_title").into_owned();
    let default_name_for_commit = default_name.clone();

    super::playlist_editor::show_smart_playlist_editor(win, &default_name, None, move |rules| {
        let win = win_for_result.clone();
        let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
        let default_name = default_name_for_commit.clone();
        let (result_tx, result_rx) = async_channel::bounded::<PlaylistCrudOutcome<()>>(1);

        rt_handle.spawn(async move {
            let outcome = match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    match mgr.create_smart_playlist(&default_name, &rules).await {
                        Ok(_) => PlaylistCrudOutcome::Committed(()),
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to create smart playlist");
                            PlaylistCrudOutcome::Failed
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB");
                    PlaylistCrudOutcome::Failed
                }
            };
            let _ = result_tx.send(outcome).await;
        });

        glib::MainContext::default().spawn_local(async move {
            match result_rx.recv().await {
                Ok(outcome)
                    if playlist_sidebar_publication_effect(&outcome)
                        == PlaylistSidebarPublicationEffect::RequestFullSnapshot =>
                {
                    request_playlist_sidebar_refresh(&playlist_sidebar_refresh);
                }
                Ok(_) | Err(_) => show_playlist_mutation_failed(&win),
            }
        });
    });
}

/// Show a rename dialog and update the playlist name in DB + sidebar.
fn handle_rename(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_sidebar_refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
    playlist_id: &str,
) {
    info!(id = %playlist_id, "Renaming playlist");
    let sidebar_store = sidebar_store.clone();
    let rt_handle = rt_handle.clone();
    let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
    let pid = playlist_id.to_string();
    let win_for_result = win.clone();

    let dialog = adw::AlertDialog::builder()
        .heading(rust_i18n::t!("dialogs.rename_playlist_heading").as_ref())
        .close_response("cancel")
        .default_response("rename")
        .build();
    dialog.add_response("cancel", rust_i18n::t!("dialogs.cancel").as_ref());
    dialog.add_response("rename", rust_i18n::t!("dialogs.rename").as_ref());
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let name_entry = gtk::Entry::builder()
        .placeholder_text(rust_i18n::t!("dialogs.new_name").as_ref())
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&name_entry));

    dialog.connect_response(None, move |_dialog, response| {
        if response != "rename" {
            return;
        }
        let new_name = name_entry.text().to_string();
        if new_name.is_empty() {
            return;
        }
        // A dialog can outlive the row that opened it. Re-resolve the exact
        // current row so a recycled or newly-linked playlist cannot retain a
        // stale ordinary action.
        if !playlist_allows_ordinary_actions(&sidebar_store, &pid) {
            show_playlist_mutation_failed(&win_for_result);
            return;
        }

        let win = win_for_result.clone();
        let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
        let pid_for_db = pid.clone();
        let (result_tx, result_rx) = async_channel::bounded::<PlaylistCrudOutcome<()>>(1);

        rt_handle.spawn(async move {
            let outcome = match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    match mgr.rename_playlist(&pid_for_db, &new_name).await {
                        Ok(()) => PlaylistCrudOutcome::Committed(()),
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to rename playlist");
                            PlaylistCrudOutcome::Failed
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB");
                    PlaylistCrudOutcome::Failed
                }
            };
            let _ = result_tx.send(outcome).await;
        });

        glib::MainContext::default().spawn_local(async move {
            match result_rx.recv().await {
                Ok(outcome)
                    if playlist_sidebar_publication_effect(&outcome)
                        == PlaylistSidebarPublicationEffect::RequestFullSnapshot =>
                {
                    request_playlist_sidebar_refresh(&playlist_sidebar_refresh);
                }
                Ok(_) | Err(_) => show_playlist_mutation_failed(&win),
            }
        });
    });

    dialog.present(Some(win));
}

/// Delete a playlist from the DB and remove from sidebar.
fn handle_delete(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_sidebar_refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
    playlist_id: &str,
) {
    info!(id = %playlist_id, "Deleting playlist");
    let sidebar_store = sidebar_store.clone();
    let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
    let win = win.clone();
    let pid = playlist_id.to_string();

    if !playlist_allows_ordinary_actions(&sidebar_store, &pid) {
        return;
    }

    let (result_tx, result_rx) = async_channel::bounded::<PlaylistCrudOutcome<()>>(1);

    rt_handle.spawn(async move {
        let outcome = match crate::db::connection::init_db().await {
            Ok(db) => {
                let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                match mgr.delete_playlist(&pid).await {
                    Ok(()) => PlaylistCrudOutcome::Committed(()),
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to delete playlist");
                        PlaylistCrudOutcome::Failed
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to open DB");
                PlaylistCrudOutcome::Failed
            }
        };
        let _ = result_tx.send(outcome).await;
    });

    glib::MainContext::default().spawn_local(async move {
        match result_rx.recv().await {
            Ok(outcome)
                if playlist_sidebar_publication_effect(&outcome)
                    == PlaylistSidebarPublicationEffect::RequestFullSnapshot =>
            {
                request_playlist_sidebar_refresh(&playlist_sidebar_refresh);
            }
            Ok(_) | Err(_) => show_playlist_mutation_failed(&win),
        }
    });
}

/// Fetch existing smart rules from DB, show the editor, and save updates.
fn handle_edit_smart(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_sidebar_refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
    playlist_id: &str,
) {
    info!(id = %playlist_id, "Editing smart playlist rules");
    let sidebar_store = sidebar_store.clone();
    let rt_handle = rt_handle.clone();
    let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
    let pid = playlist_id.to_string();
    let win = win.clone();

    // Fetch existing rules from DB.
    let (rules_tx, rules_rx) = async_channel::bounded::<
        PlaylistCrudOutcome<(String, Option<crate::local::smart_rules::SmartRules>)>,
    >(1);

    let pid_fetch = pid.clone();
    rt_handle.spawn(async move {
        let outcome = match crate::db::connection::init_db().await {
            Ok(db) => {
                let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                match mgr.get_playlist(&pid_fetch).await {
                    Ok(Some(pl)) => {
                        let rules = match pl.smart_rules_json.as_deref() {
                            Some(json) => match serde_json::from_str(json) {
                                Ok(rules) => Some(rules),
                                Err(error) => {
                                    tracing::error!(%error, id = %pid_fetch, "Failed to parse smart playlist rules");
                                    let _ = rules_tx.send(PlaylistCrudOutcome::Failed).await;
                                    return;
                                }
                            },
                            None => None,
                        };
                        PlaylistCrudOutcome::Committed((pl.name, rules))
                    }
                    Ok(None) => {
                        tracing::error!(id = %pid_fetch, "Smart playlist no longer exists");
                        PlaylistCrudOutcome::Failed
                    }
                    Err(error) => {
                        tracing::error!(%error, id = %pid_fetch, "Failed to load smart playlist");
                        PlaylistCrudOutcome::Failed
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to open DB");
                PlaylistCrudOutcome::Failed
            }
        };
        let _ = rules_tx.send(outcome).await;
    });

    glib::MainContext::default().spawn_local(async move {
        match rules_rx.recv().await {
            Ok(PlaylistCrudOutcome::Committed((name, existing_rules))) => {
                let rt_handle = rt_handle.clone();
                let pid = pid.clone();
                let sidebar_store = sidebar_store.clone();
                let win_for_result = win.clone();
                let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();

                super::playlist_editor::show_smart_playlist_editor(
                    &win,
                    &name,
                    existing_rules.as_ref(),
                    move |rules| {
                        if !playlist_is_editable_smart(&sidebar_store, &pid) {
                            show_playlist_mutation_failed(&win_for_result);
                            return;
                        }
                        let pid = pid.clone();
                        let win = win_for_result.clone();
                        let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
                        let (result_tx, result_rx) =
                            async_channel::bounded::<PlaylistCrudOutcome<()>>(1);
                        rt_handle.spawn(async move {
                            let outcome = match crate::db::connection::init_db().await {
                                Ok(db) => {
                                    let mgr =
                                        crate::local::playlist_manager::PlaylistManager::new(db);
                                    match mgr.set_smart_rules(&pid, &rules).await {
                                        Ok(()) => {
                                            info!(id = %pid, "Smart playlist rules saved");
                                            PlaylistCrudOutcome::Committed(())
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "Failed to save smart rules");
                                            PlaylistCrudOutcome::Failed
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to open DB");
                                    PlaylistCrudOutcome::Failed
                                }
                            };
                            let _ = result_tx.send(outcome).await;
                        });
                        glib::MainContext::default().spawn_local(async move {
                            match result_rx.recv().await {
                                Ok(outcome)
                                    if playlist_sidebar_publication_effect(&outcome)
                                        == PlaylistSidebarPublicationEffect::RequestFullSnapshot =>
                                {
                                    request_playlist_sidebar_refresh(&playlist_sidebar_refresh);
                                }
                                Ok(_) | Err(_) => {
                                    show_playlist_mutation_failed(&win);
                                }
                            }
                        });
                    },
                );
            }
            Ok(PlaylistCrudOutcome::Failed) | Err(_) => show_playlist_mutation_failed(&win),
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════
// Shared helper
// ═══════════════════════════════════════════════════════════════════════

fn playlist_source(
    sidebar_store: &gtk::gio::ListStore,
    playlist_id: &str,
) -> Option<(u32, SourceObject)> {
    (0..sidebar_store.n_items()).find_map(|position| {
        let source = sidebar_store
            .item(position)?
            .downcast::<SourceObject>()
            .ok()?;
        (source.is_playlist() && source.playlist_id() == playlist_id).then_some((position, source))
    })
}

fn playlist_allows_ordinary_actions(
    sidebar_store: &gtk::gio::ListStore,
    playlist_id: &str,
) -> bool {
    playlist_source(sidebar_store, playlist_id).is_some_and(|(_, source)| {
        matches!(
            source.playlist_kind(),
            Some(PlaylistSidebarKind::EditableRegular | PlaylistSidebarKind::EditableSmart)
        )
    })
}

fn playlist_is_editable_smart(sidebar_store: &gtk::gio::ListStore, playlist_id: &str) -> bool {
    playlist_source(sidebar_store, playlist_id).is_some_and(|(_, source)| {
        matches!(
            source.playlist_kind(),
            Some(PlaylistSidebarKind::EditableSmart)
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaylistMutationFailedCopy {
    heading: String,
    body: String,
}

fn playlist_mutation_failed_copy(locale: &str) -> PlaylistMutationFailedCopy {
    PlaylistMutationFailedCopy {
        heading: rust_i18n::t!("regular_playlist.mutation_failed_heading", locale = locale)
            .into_owned(),
        body: rust_i18n::t!("regular_playlist.mutation_failed_body", locale = locale).into_owned(),
    }
}

fn show_playlist_mutation_failed(win: &adw::ApplicationWindow) {
    let copy = playlist_mutation_failed_copy(&rust_i18n::locale());
    show_playlist_alert(win, &copy.heading, &copy.body);
}

// ═══════════════════════════════════════════════════════════════════════
// Playlist import/export
// ═══════════════════════════════════════════════════════════════════════

fn show_playlist_alert(win: &adw::ApplicationWindow, heading: &str, body: &str) {
    let alert = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    alert.add_response("ok", rust_i18n::t!("dialogs.ok").as_ref());
    alert.present(Some(win));
}

fn local_only_export_unsupported_body(locale: &str) -> String {
    rust_i18n::t!(
        "playlist_io.export_local_only_unsupported_body",
        locale = locale
    )
    .into_owned()
}

/// Opens an XSPF v1 file, parses it off the async worker threads, imports the
/// playlist transactionally, and publishes the committed playlist in the
/// sidebar together with an explicit outcome summary.
fn handle_import_playlist(
    win: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    playlist_sidebar_refresh: &crate::local::playlist_sidebar::PlaylistSidebarRefresh,
) {
    let rt = rt_handle.clone();
    let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();

    let xspf_filter = gtk::FileFilter::new();
    xspf_filter.set_name(Some(rust_i18n::t!("playlist_io.import_filter").as_ref()));
    xspf_filter.add_pattern("*.xspf");

    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&xspf_filter);

    let dialog = gtk::FileDialog::builder()
        .title(rust_i18n::t!("playlist_io.import_dialog_title").as_ref())
        .modal(true)
        .filters(&filters)
        .build();

    let win = win.clone();
    let dialog_parent = win.clone();
    dialog.open(
        Some(&dialog_parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let file = match result {
                Ok(file) => file,
                Err(error) if error.matches(gtk::gio::IOErrorEnum::Cancelled) => return,
                Err(error) => {
                    show_playlist_alert(
                        &win,
                        rust_i18n::t!("playlist_io.import_chooser_failed_heading").as_ref(),
                        rust_i18n::t!("playlist_io.file_chooser_failed", error = error).as_ref(),
                    );
                    return;
                }
            };
            let Some(path) = file.path() else {
                show_playlist_alert(
                    &win,
                    rust_i18n::t!("playlist_io.import_local_path_heading").as_ref(),
                    rust_i18n::t!("playlist_io.import_local_path_body").as_ref(),
                );
                return;
            };

            info!(path = %path.display(), "Importing XSPF playlist");

            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| {
                    rust_i18n::t!("playlist_io.imported_playlist_name").into_owned()
                });

            // Carry an explicit committed result or a user-visible error back
            // to GTK. A dropped sender is handled as a failure too.
            let (result_tx, result_rx) = tokio::sync::oneshot::channel::<
                Result<(String, crate::local::playlist_manager::PlaylistImportCounts), String>,
            >();

            let path_clone = path.clone();
            let name_clone = name.clone();
            rt.spawn(async move {
                let outcome: Result<_, String> = async {
                    let imported = tokio::task::spawn_blocking(move || {
                        crate::local::playlist_io::import_xspf(&path_clone)
                    })
                    .await
                    .map_err(|error| {
                        rust_i18n::t!("playlist_io.parser_worker_failed", error = error)
                            .into_owned()
                    })?
                    .map_err(|error| {
                        rust_i18n::t!("playlist_io.parse_failed", error = error).into_owned()
                    })?;

                    let db = crate::db::connection::init_db().await.map_err(|error| {
                        rust_i18n::t!("playlist_io.database_open_failed", error = error)
                            .into_owned()
                    })?;
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    let result = mgr
                        .import_regular_playlist(&name_clone, &imported)
                        .await
                        .map_err(|error| {
                            rust_i18n::t!("playlist_io.import_commit_failed", error = error)
                                .into_owned()
                        })?;

                    Ok((result.playlist.name, result.counts))
                }
                .await;

                if let Err(error) = &outcome {
                    tracing::error!(error = %error, "XSPF playlist import failed");
                }
                let _ = result_tx.send(outcome);
            });

            // Receive the committed result on the GTK main thread. Only this
            // success branch requests a complete authoritative replacement.
            let win = win.clone();
            let playlist_sidebar_refresh = playlist_sidebar_refresh.clone();
            glib::MainContext::default().spawn_local(async move {
                match result_rx.await {
                    Ok(Ok((pname, counts))) => {
                        info!(
                            name = %pname,
                            matched = counts.matched,
                            unmatched = counts.unmatched,
                            failed = counts.failed,
                            "XSPF playlist import complete"
                        );
                        let body = rust_i18n::t!(
                            "playlist_io.import_success_body",
                            name = pname,
                            matched = counts.matched,
                            unmatched = counts.unmatched,
                            failed = counts.failed
                        );
                        let heading = if counts.failed == 0 {
                            rust_i18n::t!("playlist_io.import_success_heading")
                        } else {
                            rust_i18n::t!("playlist_io.import_warning_heading")
                        };
                        // The manager returned only after the playlist and
                        // all retained entries committed together. The new row
                        // still comes only from the engine-owned publication.
                        request_playlist_sidebar_refresh(&playlist_sidebar_refresh);
                        show_playlist_alert(&win, heading.as_ref(), body.as_ref());
                    }
                    Ok(Err(error)) => show_playlist_alert(
                        &win,
                        rust_i18n::t!("playlist_io.import_failed_heading").as_ref(),
                        rust_i18n::t!("playlist_io.import_rollback_body", error = error).as_ref(),
                    ),
                    Err(error) => show_playlist_alert(
                        &win,
                        rust_i18n::t!("playlist_io.import_failed_heading").as_ref(),
                        rust_i18n::t!("playlist_io.import_worker_failed_body", error = error)
                            .as_ref(),
                    ),
                }
            });
        },
    );
}

/// Export a playlist to an XSPF file.
///
/// Shows a save dialog first (GTK main thread), fetches tracks asynchronously,
/// and performs the blocking atomic XSPF write on a blocking worker.
fn handle_export_playlist(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_id: &str,
) {
    let rt = rt_handle.clone();
    let pid = playlist_id.to_string();

    if !playlist_allows_ordinary_actions(sidebar_store, &pid) {
        return;
    }
    let sidebar_store = sidebar_store.clone();

    let xspf_filter = gtk::FileFilter::new();
    xspf_filter.set_name(Some(rust_i18n::t!("playlist_io.export_filter").as_ref()));
    xspf_filter.add_pattern("*.xspf");

    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&xspf_filter);

    let dialog = gtk::FileDialog::builder()
        .title(rust_i18n::t!("playlist_io.export_dialog_title").as_ref())
        .modal(true)
        .initial_name(rust_i18n::t!("playlist_io.export_filename").as_ref())
        .filters(&filters)
        .build();

    let win = win.clone();
    let dialog_parent = win.clone();
    dialog.save(
        Some(&dialog_parent),
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let file = match result {
                Ok(file) => file,
                Err(error) if error.matches(gtk::gio::IOErrorEnum::Cancelled) => return,
                Err(error) => {
                    show_playlist_alert(
                        &win,
                        rust_i18n::t!("playlist_io.export_chooser_failed_heading").as_ref(),
                        rust_i18n::t!("playlist_io.file_chooser_failed", error = error).as_ref(),
                    );
                    return;
                }
            };
            let Some(path) = file.path() else {
                show_playlist_alert(
                    &win,
                    rust_i18n::t!("playlist_io.export_local_path_heading").as_ref(),
                    rust_i18n::t!("playlist_io.export_local_path_body").as_ref(),
                );
                return;
            };
            // Re-resolve after the chooser closes. The row may have been
            // removed or become a linked mirror while the dialog was open.
            if !playlist_allows_ordinary_actions(&sidebar_store, &pid) {
                show_playlist_mutation_failed(&win);
                return;
            }

            // Fetch tracks asynchronously, then isolate XML generation and
            // atomic filesystem replacement from async workers.
            let path = path.clone();
            let pid = pid.clone();
            let (result_tx, result_rx) =
                tokio::sync::oneshot::channel::<Result<std::path::PathBuf, String>>();
            rt.spawn(async move {
                let outcome: Result<_, String> = async {
                    let db = crate::db::connection::init_db().await.map_err(|error| {
                        rust_i18n::t!("playlist_io.database_open_failed", error = error)
                            .into_owned()
                    })?;
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    let playlist = mgr
                        .get_playlist(&pid)
                        .await
                        .map_err(|error| {
                            rust_i18n::t!("playlist_io.playlist_read_failed", error = error)
                                .into_owned()
                        })?
                        .ok_or_else(|| {
                            rust_i18n::t!("playlist_io.playlist_missing").into_owned()
                        })?;

                    // Smart playlists are evaluated from the current library;
                    // regular playlists use their persisted reconciled entries.
                    let tracks = if playlist.is_smart {
                        mgr.evaluate_smart_playlist(&pid).await.map_err(|error| {
                            rust_i18n::t!(
                                "playlist_io.smart_playlist_evaluation_failed",
                                error = error
                            )
                            .into_owned()
                        })?
                    } else {
                        match mgr.local_playlist_export(&pid).await.map_err(|error| {
                            rust_i18n::t!("playlist_io.playlist_tracks_read_failed", error = error)
                                .into_owned()
                        })? {
                            crate::local::playlist_manager::LocalPlaylistExport::Ready(tracks) => {
                                tracks
                            }
                            crate::local::playlist_manager::LocalPlaylistExport::UnsupportedEntries => {
                                return Err(local_only_export_unsupported_body(
                                    rust_i18n::locale().as_ref(),
                                ));
                            }
                        }
                    };

                    let export_path = path.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::local::playlist_io::export_xspf(&tracks, &export_path)
                    })
                    .await
                    .map_err(|error| {
                        rust_i18n::t!("playlist_io.writer_worker_failed", error = error)
                            .into_owned()
                    })?
                    .map_err(|error| {
                        rust_i18n::t!("playlist_io.write_failed", error = error).into_owned()
                    })?;

                    Ok(path)
                }
                .await;

                if let Err(error) = &outcome {
                    tracing::error!(error = %error, "XSPF playlist export failed");
                }
                let _ = result_tx.send(outcome);
            });

            let win = win.clone();
            glib::MainContext::default().spawn_local(async move {
                match result_rx.await {
                    Ok(Ok(path)) => {
                        info!(path = %path.display(), "XSPF playlist exported");
                        show_playlist_alert(
                            &win,
                            rust_i18n::t!("playlist_io.export_success_heading").as_ref(),
                            rust_i18n::t!("playlist_io.export_success_body", path = path.display())
                                .as_ref(),
                        );
                    }
                    Ok(Err(error)) => show_playlist_alert(
                        &win,
                        rust_i18n::t!("playlist_io.export_failed_heading").as_ref(),
                        rust_i18n::t!("playlist_io.export_unchanged_body", error = error).as_ref(),
                    ),
                    Err(error) => show_playlist_alert(
                        &win,
                        rust_i18n::t!("playlist_io.export_failed_heading").as_ref(),
                        rust_i18n::t!("playlist_io.export_worker_failed_body", error = error)
                            .as_ref(),
                    ),
                }
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entities::server_playlist_link::{
        ServerPlaylistLocalState, ServerPlaylistRemoteState,
    };
    use crate::local::playlist_sidebar::PlaylistSidebarEntry;

    fn playlist_source_for_test(id: &str, name: &str, kind: PlaylistSidebarKind) -> SourceObject {
        SourceObject::playlist_entry(&PlaylistSidebarEntry::new(id, name, kind))
    }

    #[test]
    fn failed_or_dropped_mutation_has_no_optimistic_sidebar_rollback_effect() {
        assert_eq!(
            playlist_sidebar_publication_effect(&PlaylistCrudOutcome::<()>::Failed),
            PlaylistSidebarPublicationEffect::LeavePublishedSnapshot
        );
        assert_eq!(
            playlist_sidebar_publication_effect(&PlaylistCrudOutcome::Committed(())),
            PlaylistSidebarPublicationEffect::RequestFullSnapshot
        );
    }

    #[test]
    fn pull_mirrors_reject_ordinary_and_smart_edit_actions() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        store.append(&playlist_source_for_test(
            "regular-id",
            "Regular",
            PlaylistSidebarKind::EditableRegular,
        ));
        store.append(&playlist_source_for_test(
            "smart-id",
            "Smart",
            PlaylistSidebarKind::EditableSmart,
        ));
        store.append(&playlist_source_for_test(
            "mirror-id",
            "Mirror",
            PlaylistSidebarKind::PullMirror {
                local_state: ServerPlaylistLocalState::Conflict,
                remote_state: ServerPlaylistRemoteState::Missing,
            },
        ));

        assert!(playlist_allows_ordinary_actions(&store, "regular-id"));
        assert!(playlist_allows_ordinary_actions(&store, "smart-id"));
        assert!(!playlist_allows_ordinary_actions(&store, "mirror-id"));
        assert!(playlist_is_editable_smart(&store, "smart-id"));
        assert!(!playlist_is_editable_smart(&store, "regular-id"));
        assert!(!playlist_is_editable_smart(&store, "mirror-id"));
        assert!(!playlist_allows_ordinary_actions(&store, "missing-id"));
    }

    #[test]
    fn playlist_mutation_failure_copy_is_localized_for_every_catalog() {
        let english = playlist_mutation_failed_copy("en");
        assert!(!english.heading.is_empty());
        assert!(!english.body.is_empty());

        for locale in rust_i18n::available_locales!() {
            let localized = playlist_mutation_failed_copy(&locale);
            assert!(!localized.heading.is_empty(), "{locale}: empty heading");
            assert!(!localized.body.is_empty(), "{locale}: empty body");
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }

    #[test]
    fn local_only_export_refusal_is_localized_without_english_fallback() {
        let english = local_only_export_unsupported_body("en");
        assert!(!english.is_empty());

        for locale in rust_i18n::available_locales!() {
            let localized = local_only_export_unsupported_body(&locale);
            assert!(!localized.is_empty(), "{locale}: empty refusal copy");
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }
}
