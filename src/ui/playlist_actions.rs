//! Playlist sidebar CRUD actions (create, rename, delete, edit smart rules).
//!
//! Handles [`sidebar::PlaylistAction`] events received from the sidebar
//! context menu and wires them to database operations via the
//! [`PlaylistManager`](crate::local::playlist_manager::PlaylistManager).

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use super::objects::SourceObject;
use super::sidebar;
use super::window_state::WindowState;

/// Wire the playlist action receiver to the sidebar store.
///
/// Spawns an async task on the GTK main context that listens for
/// [`sidebar::PlaylistAction`] events and dispatches to the appropriate
/// dialog + DB handler.
pub fn setup_playlist_actions(
    state: &WindowState,
    playlist_action_rx: async_channel::Receiver<sidebar::PlaylistAction>,
) {
    let sidebar_store = state.sidebar_store.clone();
    let rt_handle = state.rt_handle.clone();
    let win = state.window.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(action) = playlist_action_rx.recv().await {
            match action {
                sidebar::PlaylistAction::CreateRegular => {
                    handle_create_regular(&win, &sidebar_store, &rt_handle);
                }

                sidebar::PlaylistAction::CreateSmart => {
                    handle_create_smart(&win, &sidebar_store, &rt_handle);
                }

                sidebar::PlaylistAction::Rename(playlist_id) => {
                    handle_rename(&win, &sidebar_store, &rt_handle, &playlist_id);
                }

                sidebar::PlaylistAction::Delete(playlist_id) => {
                    handle_delete(&sidebar_store, &rt_handle, &playlist_id);
                }

                sidebar::PlaylistAction::EditSmart(playlist_id) => {
                    handle_edit_smart(&win, &sidebar_store, &rt_handle, &playlist_id);
                }
            }
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════
// Individual action handlers
// ═══════════════════════════════════════════════════════════════════════

/// Show a name dialog and create a new regular playlist.
fn handle_create_regular(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
) {
    info!("Creating new regular playlist");
    let sidebar_store = sidebar_store.clone();
    let rt_handle = rt_handle.clone();

    let dialog = adw::AlertDialog::builder()
        .heading("New Playlist")
        .close_response("cancel")
        .default_response("create")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create");
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);

    let name_entry = gtk::Entry::builder()
        .placeholder_text("Playlist name")
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

        let sidebar_store = sidebar_store.clone();
        let (result_tx, result_rx) = async_channel::bounded::<(String, String, bool)>(1);

        rt_handle.spawn(async move {
            match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    match mgr.create_playlist(&name, false).await {
                        Ok(pl) => {
                            let _ = result_tx.send((pl.id, pl.name, pl.is_smart)).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to create playlist");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB");
                }
            }
        });

        glib::MainContext::default().spawn_local(async move {
            if let Ok((id, name, is_smart)) = result_rx.recv().await {
                insert_playlist_into_sidebar(&sidebar_store, &name, &id, is_smart);
            }
        });
    });

    dialog.present(Some(win));
}

/// Open the smart playlist editor and create a new smart playlist.
fn handle_create_smart(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
) {
    info!("Creating new smart playlist");
    let sidebar_store = sidebar_store.clone();
    let rt_handle = rt_handle.clone();

    super::playlist_editor::show_smart_playlist_editor(win, "Untitled", None, move |rules| {
        let sidebar_store = sidebar_store.clone();
        let (result_tx, result_rx) = async_channel::bounded::<(String, String, bool)>(1);

        rt_handle.spawn(async move {
            match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    match mgr.create_playlist("Smart Playlist", true).await {
                        Ok(pl) => {
                            let _ = mgr.set_smart_rules(&pl.id, &rules).await;
                            let _ = result_tx.send((pl.id, pl.name, pl.is_smart)).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to create smart playlist");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB");
                }
            }
        });

        glib::MainContext::default().spawn_local(async move {
            if let Ok((id, name, is_smart)) = result_rx.recv().await {
                insert_playlist_into_sidebar(&sidebar_store, &name, &id, is_smart);
            }
        });
    });
}

/// Show a rename dialog and update the playlist name in DB + sidebar.
fn handle_rename(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_id: &str,
) {
    info!(id = %playlist_id, "Renaming playlist");
    let sidebar_store = sidebar_store.clone();
    let rt_handle = rt_handle.clone();
    let pid = playlist_id.to_string();

    let dialog = adw::AlertDialog::builder()
        .heading("Rename Playlist")
        .close_response("cancel")
        .default_response("rename")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let name_entry = gtk::Entry::builder()
        .placeholder_text("New name")
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

        let sidebar_store = sidebar_store.clone();
        let pid_for_db = pid.clone();
        let pid_for_ui = pid.clone();
        let new_name_for_ui = new_name.clone();
        let (done_tx, done_rx) = async_channel::bounded::<()>(1);

        rt_handle.spawn(async move {
            match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                    if let Err(e) = mgr.rename_playlist(&pid_for_db, &new_name).await {
                        tracing::error!(error = %e, "Failed to rename playlist");
                    }
                    let _ = done_tx.send(()).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB");
                }
            }
        });

        glib::MainContext::default().spawn_local(async move {
            if done_rx.recv().await.is_ok() {
                // Update sidebar entry name.
                for i in 0..sidebar_store.n_items() {
                    if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
                        if src.playlist_id() == pid_for_ui {
                            let is_smart = src.backend_type() == "smart-playlist";
                            let new_src =
                                SourceObject::playlist(&new_name_for_ui, &pid_for_ui, is_smart);
                            sidebar_store.remove(i);
                            sidebar_store.insert(i, &new_src);
                            break;
                        }
                    }
                }
            }
        });
    });

    dialog.present(Some(win));
}

/// Delete a playlist from the DB and remove from sidebar.
fn handle_delete(
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_id: &str,
) {
    info!(id = %playlist_id, "Deleting playlist");
    let sidebar_store = sidebar_store.clone();
    let rt_handle_clone = rt_handle.clone();
    let pid = playlist_id.to_string();

    let (done_tx, done_rx) = async_channel::bounded::<()>(1);

    rt_handle.spawn(async move {
        match crate::db::connection::init_db().await {
            Ok(db) => {
                let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                if let Err(e) = mgr.delete_playlist(&pid).await {
                    tracing::error!(error = %e, "Failed to delete playlist");
                }
                let _ = done_tx.send(()).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to open DB");
            }
        }
    });

    let pid = playlist_id.to_string();
    glib::MainContext::default().spawn_local(async move {
        if done_rx.recv().await.is_ok() {
            for i in 0..sidebar_store.n_items() {
                if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
                    if src.playlist_id() == pid {
                        sidebar_store.remove(i);
                        break;
                    }
                }
            }
        }
    });
    // Suppress unused variable warning — rt_handle_clone is used to
    // ensure the Handle stays alive for the spawned future.
    let _ = rt_handle_clone;
}

/// Fetch existing smart rules from DB, show the editor, and save updates.
fn handle_edit_smart(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
    playlist_id: &str,
) {
    info!(id = %playlist_id, "Editing smart playlist rules");
    let _sidebar_store = sidebar_store.clone();
    let rt_handle = rt_handle.clone();
    let pid = playlist_id.to_string();
    let win = win.clone();

    // Fetch existing rules from DB.
    let (rules_tx, rules_rx) =
        async_channel::bounded::<(String, Option<crate::local::smart_rules::SmartRules>)>(1);

    let pid_fetch = pid.clone();
    rt_handle.spawn(async move {
        match crate::db::connection::init_db().await {
            Ok(db) => {
                let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                match mgr.get_playlist(&pid_fetch).await {
                    Ok(Some(pl)) => {
                        let rules = pl
                            .smart_rules_json
                            .as_deref()
                            .and_then(|j| serde_json::from_str(j).ok());
                        let _ = rules_tx.send((pl.name, rules)).await;
                    }
                    _ => {
                        let _ = rules_tx.send(("Smart Playlist".to_string(), None)).await;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to open DB");
            }
        }
    });

    glib::MainContext::default().spawn_local(async move {
        if let Ok((name, existing_rules)) = rules_rx.recv().await {
            let rt_handle = rt_handle.clone();
            let pid = pid.clone();

            super::playlist_editor::show_smart_playlist_editor(
                &win,
                &name,
                existing_rules.as_ref(),
                move |rules| {
                    let pid = pid.clone();
                    rt_handle.spawn(async move {
                        match crate::db::connection::init_db().await {
                            Ok(db) => {
                                let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                                if let Err(e) = mgr.set_smart_rules(&pid, &rules).await {
                                    tracing::error!(error = %e, "Failed to save smart rules");
                                } else {
                                    info!(id = %pid, "Smart playlist rules saved");
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to open DB");
                            }
                        }
                    });
                },
            );
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════
// Shared helper
// ═══════════════════════════════════════════════════════════════════════

/// Insert a new playlist `SourceObject` into the sidebar store under the
/// "Playlists" header, at the end of the playlists section.
fn insert_playlist_into_sidebar(
    sidebar_store: &gtk::gio::ListStore,
    name: &str,
    id: &str,
    is_smart: bool,
) {
    let src = SourceObject::playlist(name, id, is_smart);
    let n = sidebar_store.n_items();
    for i in 0..n {
        if let Some(s) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
            if s.is_header() && s.name() == "Playlists" {
                // Find end of playlists section.
                let mut pos = i + 1;
                while pos < sidebar_store.n_items() {
                    if let Some(next) = sidebar_store.item(pos).and_downcast_ref::<SourceObject>() {
                        if next.is_header() {
                            break;
                        }
                        let bt = next.backend_type();
                        if bt == "playlist" || bt == "smart-playlist" {
                            pos += 1;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                sidebar_store.insert(pos, &src);
                break;
            }
        }
    }
}
