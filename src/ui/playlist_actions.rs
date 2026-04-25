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

                sidebar::PlaylistAction::ImportPlaylist => {
                    handle_import_playlist(&win, &sidebar_store, &rt_handle);
                }

                sidebar::PlaylistAction::ExportPlaylist(playlist_id) => {
                    handle_export_playlist(&win, &rt_handle, &playlist_id);
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

// ═══════════════════════════════════════════════════════════════════════
// Playlist import/export
// ═══════════════════════════════════════════════════════════════════════

/// Opens a file dialog, parses the XSPF, matches tracks against the local
/// library via fingerprinting, creates a new regular playlist, and updates
/// the sidebar.
fn handle_import_playlist(
    win: &adw::ApplicationWindow,
    sidebar_store: &gtk::gio::ListStore,
    rt_handle: &tokio::runtime::Handle,
) {
    let sidebar_store = sidebar_store.clone();
    let rt = rt_handle.clone();

    let xspf_filter = gtk::FileFilter::new();
    xspf_filter.set_name(Some("XSPF Playlists"));
    xspf_filter.add_pattern("*.xspf");

    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&xspf_filter);

    let dialog = gtk::FileDialog::builder()
        .title("Import Playlist")
        .modal(true)
        .filters(&filters)
        .build();

    let win = win.clone();
    dialog.open(Some(&win), None::<&gtk::gio::Cancellable>, move |result| {
        let Ok(file) = result else { return };
        let Some(path) = file.path() else { return };

        info!(path = %path.display(), "Importing XSPF playlist");

        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Imported Playlist".to_string());

        // Channel: send (playlist_name, playlist_id) back to GTK thread.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<Option<(String, String)>>();

        let path_clone = path.clone();
        let name_clone = name.clone();
        rt.spawn(async move {
            let imported = match crate::local::playlist_io::import_xspf(&path_clone) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to parse XSPF");
                    let _ = result_tx.send(None);
                    return;
                }
            };

            let db = match crate::db::connection::init_db().await {
                Ok(db) => db,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB for import");
                    let _ = result_tx.send(None);
                    return;
                }
            };

            let (matched, _unmatched) =
                crate::local::playlist_io::match_imported_tracks(&db, &imported).await;
            let matched_count = matched.len();

            let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
            let playlist = match mgr.create_playlist(&name_clone, false).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create playlist");
                    let _ = result_tx.send(None);
                    return;
                }
            };

            for track in &matched {
                let _ = mgr.add_track(&playlist.id, track).await;
            }

            info!(name = %name_clone, matched = matched_count, "Playlist import complete");
            let _ = result_tx.send(Some((playlist.name.clone(), playlist.id.clone())));
        });

        // Receive the result on the GTK main thread.
        glib::MainContext::default().spawn_local(async move {
            if let Ok(Some((pname, pid))) = result_rx.await {
                insert_playlist_into_sidebar(&sidebar_store, &pname, &pid, false);
            }
        });
    });
}

/// Export a playlist to an XSPF file.
///
/// Shows a save dialog first (GTK main thread), then fetches tracks
/// from the database and writes the XSPF file on the async runtime.
fn handle_export_playlist(
    win: &adw::ApplicationWindow,
    rt_handle: &tokio::runtime::Handle,
    playlist_id: &str,
) {
    let rt = rt_handle.clone();
    let pid = playlist_id.to_string();

    let xspf_filter = gtk::FileFilter::new();
    xspf_filter.set_name(Some("XSPF Playlist"));
    xspf_filter.add_pattern("*.xspf");

    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&xspf_filter);

    let dialog = gtk::FileDialog::builder()
        .title("Export Playlist")
        .modal(true)
        .initial_name("playlist.xspf")
        .filters(&filters)
        .build();

    let win = win.clone();
    dialog.save(Some(&win), None::<&gtk::gio::Cancellable>, move |result| {
        let Ok(file) = result else { return };
        let Some(path) = file.path() else { return };

        // User picked a path — now fetch tracks and write on the async runtime.
        let path = path.clone();
        let pid = pid.clone();
        rt.spawn(async move {
            let db = match crate::db::connection::init_db().await {
                Ok(db) => db,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB for export");
                    return;
                }
            };

            let mgr = crate::local::playlist_manager::PlaylistManager::new(db.clone());

            let Ok(Some(playlist)) = mgr.get_playlist(&pid).await else {
                tracing::error!(id = %pid, "Playlist not found for export");
                return;
            };

            // Get tracks: if smart, evaluate; if regular, get entries.
            let tracks = if playlist.is_smart {
                match mgr.evaluate_smart_playlist(&pid).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to evaluate smart playlist");
                        return;
                    }
                }
            } else {
                match mgr.get_playlist_tracks(&pid).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to get playlist tracks");
                        return;
                    }
                }
            };

            match crate::local::playlist_io::export_xspf(&tracks, &path) {
                Ok(()) => {
                    info!(path = %path.display(), "Playlist exported");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to export playlist");
                }
            }
        });
    });
}
