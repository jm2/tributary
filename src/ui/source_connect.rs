//! Sidebar selection-changed handler — source switching + auth dialogs.
//!
//! Handles clicking sidebar items: switching between local, playlist,
//! USB, radio, connected remote, and unauthenticated remote sources.

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use crate::local::engine::LibraryEvent;

use super::objects::{SourceObject, TrackObject};
use super::preferences;
use super::radio::{
    apply_radio_columns, handle_radio_nearme, is_radio_backend, radio_station_to_track_object,
};
use super::server_dialogs::show_auth_dialog;
use super::tracklist;
use super::window::{arch_track_to_object, display_tracks};
use super::window_state::WindowState;

/// Wire the sidebar selection-changed signal.
///
/// This function owns all the logic for switching between sources when
/// the user clicks a sidebar row: local library, playlists, USB devices,
/// internet radio, already-connected remote servers, and unauthenticated
/// servers (which trigger auth dialogs).
pub fn setup_source_connect(state: &WindowState) {
    let sel = state.sidebar_selection.clone();
    let engine_tx = state.engine_tx.clone();
    let rt_handle = state.rt_handle.clone();
    let win = state.window.clone();
    let track_store = state.track_store.clone();
    let master_tracks = state.master_tracks.clone();
    let source_tracks = state.source_tracks.clone();
    let active_source_key = state.active_source_key.clone();
    let browser_widget = state.browser_widget.clone();
    let browser_state = state.browser_state.clone();
    let status_label = state.status_label.clone();
    let column_view = state.column_view.clone();
    let current_pos = state.current_pos.clone();
    let app_config = state.app_config.clone();
    let sidebar_store = state.sidebar_store.clone();
    let pending_connection = state.pending_connection.clone();
    let pre_connect_selection = state.pre_connect_selection.clone();

    sel.connect_selection_changed(move |sel, _, _| {
        let Some(item) = sel.selected_item() else {
            return;
        };
        let Some(src) = item.downcast_ref::<SourceObject>() else {
            return;
        };
        if src.is_header() {
            return;
        }

        // ── Connection guard: ignore clicks while a connection is pending ──
        // This prevents duplicate auth dialogs and duplicate connection
        // tasks when the user clicks a server that is still connecting
        // (applies to all network sources: Subsonic, Jellyfin, Plex, DAAP).
        if pending_connection.borrow().is_some() {
            let pending_url = pending_connection.borrow().clone();
            if let Some(ref pu) = pending_url {
                // If clicking the same server that's already connecting, just ignore.
                if src.server_url() == *pu {
                    return;
                }
                // If clicking a different server while one is connecting,
                // also ignore — let the first connection finish first.
                if src.connecting() || (!src.connected() && !src.server_url().is_empty()) {
                    return;
                }
            }
        }

        let backend_type = src.backend_type();

        // Determine the source key.
        // Remote sources use their server URL; local sources with a
        // specific backend_type (radio, playlist) use that type as
        // the key so they don't all collapse into "local".
        let url = src.server_url();
        let key = if !url.is_empty() {
            url.clone()
        } else if backend_type == "local" || backend_type.is_empty() {
            "local".to_string()
        } else {
            backend_type.clone()
        };

        // ── Local source: switch to local view ───────────────────
        if key == "local" {
            *active_source_key.borrow_mut() = "local".to_string();

            // Restore music column layout if coming from radio.
            apply_radio_columns(&column_view, false);
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let st = source_tracks.borrow();
            let local_tracks = st.get("local").cloned().unwrap_or_default();
            display_tracks(
                &local_tracks,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
            current_pos.set(None);
            return;
        }

        // ── Playlist source: fetch playlist tracks ───────────────
        if backend_type == "playlist" || backend_type == "smart-playlist" {
            let playlist_id = src.playlist_id();
            if playlist_id.is_empty() {
                return;
            }

            *active_source_key.borrow_mut() = format!("playlist:{playlist_id}");

            // Restore music column layout (not radio).
            apply_radio_columns(&column_view, false);
            // Restore browser visibility from config.
            let cfg = app_config.borrow();
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let is_smart = backend_type == "smart-playlist";
            let rt_handle = rt_handle.clone();
            let track_store = track_store.clone();
            let master_tracks = master_tracks.clone();
            let browser_widget = browser_widget.clone();
            let browser_state = browser_state.clone();
            let status_label = status_label.clone();
            let column_view = column_view.clone();
            let current_pos = current_pos.clone();
            let pid = playlist_id.clone();

            let (tracks_tx, tracks_rx) = async_channel::bounded::<String>(1);

            rt_handle.spawn(async move {
                match crate::db::connection::init_db().await {
                    Ok(db) => {
                        let mgr = crate::local::playlist_manager::PlaylistManager::new(db);
                        let tracks = if is_smart {
                            mgr.evaluate_smart_playlist(&pid).await
                        } else {
                            mgr.get_playlist_tracks(&pid).await
                        };
                        match tracks {
                            Ok(models) => {
                                let arch_tracks: Vec<crate::architecture::models::Track> = models
                                    .iter()
                                    .map(crate::local::engine::db_model_to_track)
                                    .collect();
                                let json = serde_json::to_string(&arch_tracks).unwrap_or_default();
                                let _ = tracks_tx.send(json).await;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to load playlist tracks");
                                let _ = tracks_tx.send("[]".to_string()).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to open DB for playlist");
                        let _ = tracks_tx.send("[]".to_string()).await;
                    }
                }
            });

            glib::MainContext::default().spawn_local(async move {
                if let Ok(json) = tracks_rx.recv().await {
                    let tracks: Vec<crate::architecture::models::Track> =
                        serde_json::from_str(&json).unwrap_or_default();
                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();
                    display_tracks(
                        &objects,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                    current_pos.set(None);
                }
            });
            return;
        }

        // ── USB device source: scan and display music files ──────
        if backend_type == "usb-device" {
            let mount_point = src.server_url();
            if mount_point.is_empty() {
                return;
            }

            *active_source_key.borrow_mut() = key.clone();

            // Restore music column layout if coming from radio.
            apply_radio_columns(&column_view, false);
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            // Check if we already scanned this device.
            let already_scanned = source_tracks.borrow().contains_key(&key);
            if already_scanned {
                let st = source_tracks.borrow();
                let tracks = st.get(&key).cloned().unwrap_or_default();
                display_tracks(
                    &tracks,
                    &track_store,
                    &master_tracks,
                    &browser_widget,
                    &browser_state,
                    &status_label,
                    &column_view,
                );
                current_pos.set(None);
            } else {
                // Scan on a background thread to avoid blocking UI.
                let mount = mount_point.clone();
                let source_key = key.clone();
                let track_store = track_store.clone();
                let master_tracks = master_tracks.clone();
                let source_tracks = source_tracks.clone();
                let browser_widget = browser_widget.clone();
                let browser_state = browser_state.clone();
                let status_label = status_label.clone();
                let column_view = column_view.clone();
                let active_source_key = active_source_key.clone();
                let current_pos = current_pos.clone();

                // Serialisable track data for cross-thread transfer.
                // TrackObject is a GObject (not Send), so we send
                // tuples of raw data from the background thread and
                // construct TrackObjects on the GTK main thread.
                type ScanRow = (
                    u32,
                    String,
                    u64,
                    String,
                    String,
                    String,
                    i32,
                    String,
                    u32,
                    u32,
                    String,
                    String,
                );
                let (scan_tx, scan_rx) = async_channel::unbounded::<ScanRow>();

                // Background thread: walk the device filesystem.
                std::thread::spawn(move || {
                    let mount_path = std::path::Path::new(&mount);
                    for entry in walkdir::WalkDir::new(mount_path)
                        .follow_links(true)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let path = entry.path();
                        if !path.is_file() {
                            continue;
                        }
                        if !crate::local::tag_parser::is_audio_file(path) {
                            continue;
                        }
                        if let Ok(parsed) = crate::local::tag_parser::parse_audio_file(path) {
                            let uri = url::Url::from_file_path(path)
                                .map(|u| u.to_string())
                                .unwrap_or_default();
                            let row: ScanRow = (
                                parsed.track_number.unwrap_or(0),
                                parsed.title,
                                parsed.duration_secs.unwrap_or(0),
                                parsed.artist_name,
                                parsed.album_title,
                                parsed.genre.unwrap_or_else(|| "Unknown".to_string()),
                                parsed.year.unwrap_or(0),
                                parsed.date_modified.format("%Y-%m-%d").to_string(),
                                parsed.bitrate_kbps.unwrap_or(0),
                                parsed.sample_rate_hz.unwrap_or(0),
                                parsed.format,
                                uri,
                            );
                            let _ = scan_tx.try_send(row);
                        }
                    }
                    // Close the sender to signal completion.
                    drop(scan_tx);
                });

                // Collect results on the GTK main thread.
                glib::MainContext::default().spawn_local(async move {
                    let mut objects = Vec::new();
                    while let Ok(row) = scan_rx.recv().await {
                        let obj = TrackObject::new(
                            row.0, &row.1, row.2, &row.3, &row.4, &row.5, row.6, &row.7, row.8,
                            row.9, 0, &row.10, &row.11,
                        );
                        objects.push(obj);
                    }

                    // Store for future source switches.
                    source_tracks
                        .borrow_mut()
                        .insert(source_key.clone(), objects.clone());

                    // Display if still the active source.
                    if *active_source_key.borrow() == source_key {
                        display_tracks(
                            &objects,
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );
                        current_pos.set(None);
                    }
                });
            }
            return;
        }

        // ── Radio source: fetch stations ────────────────────────
        if is_radio_backend(&backend_type) {
            *active_source_key.borrow_mut() = backend_type.clone();

            // Switch to radio column layout.
            apply_radio_columns(&column_view, true);
            // Hide browser for radio.
            browser_widget.set_visible(false);

            // Clear the tracklist immediately so local songs don't
            // show while radio stations are loading asynchronously.
            track_store.remove_all();
            tracklist::update_status(&status_label, &[]);
            *master_tracks.borrow_mut() = Vec::new();
            current_pos.set(None);

            // Handle "Stations Near Me" with geo consent.
            if backend_type == "radio-nearme" {
                let app_config = app_config.clone();
                let rt_handle = rt_handle.clone();
                let track_store = track_store.clone();
                let master_tracks = master_tracks.clone();
                let browser_widget = browser_widget.clone();
                let browser_state = browser_state.clone();
                let status_label = status_label.clone();
                let column_view = column_view.clone();
                let active_source_key = active_source_key.clone();
                let sidebar_selection = sel.clone();
                let current_pos = current_pos.clone();
                let source_tracks = source_tracks.clone();
                let win = win.clone();

                handle_radio_nearme(
                    &win,
                    app_config,
                    rt_handle,
                    track_store,
                    master_tracks,
                    browser_widget,
                    browser_state,
                    status_label,
                    column_view,
                    active_source_key,
                    sidebar_selection,
                    current_pos,
                    source_tracks,
                );
            } else {
                // Top Clicked or Top Voted — fetch directly.
                let bt = backend_type.clone();
                let rt_handle = rt_handle.clone();
                let track_store = track_store.clone();
                let master_tracks = master_tracks.clone();
                let browser_widget = browser_widget.clone();
                let browser_state = browser_state.clone();
                let status_label = status_label.clone();
                let column_view = column_view.clone();
                let current_pos = current_pos.clone();

                let (stations_tx, stations_rx) = async_channel::bounded::<String>(1);

                rt_handle.spawn(async move {
                    let client = crate::radio::RadioBrowserClient::new();
                    let stations = if bt == "radio-topclick" {
                        client.fetch_top_click(None).await
                    } else {
                        client.fetch_top_vote(None).await
                    };
                    let json = serde_json::to_string(&stations).unwrap_or_default();
                    let _ = stations_tx.send(json).await;
                });

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(json) = stations_rx.recv().await {
                        let stations: Vec<crate::radio::RadioStation> =
                            serde_json::from_str(&json).unwrap_or_default();
                        let objects: Vec<TrackObject> =
                            stations.iter().map(radio_station_to_track_object).collect();
                        display_tracks(
                            &objects,
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );
                        current_pos.set(None);
                    }
                });
            }
            return;
        }

        // ── Connected source: switch view ───────────────────────
        if src.connected() {
            *active_source_key.borrow_mut() = key.clone();

            // Restore music column layout if coming from radio.
            apply_radio_columns(&column_view, false);
            // Restore browser visibility from config.
            let cfg = app_config.borrow();
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let st = source_tracks.borrow();
            let tracks = st.get(&key).cloned().unwrap_or_default();
            display_tracks(
                &tracks,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
            current_pos.set(None);
            return;
        }

        // ── Discovered (unauthenticated) ────────────────────────
        let server_name = src.name();
        let server_url = src.server_url();
        let engine_tx = engine_tx.clone();
        let rt_handle = rt_handle.clone();
        let win = win.clone();
        let sidebar_store = sidebar_store.clone();
        let selected_pos = sel.selected();

        let backend_type = src.backend_type();
        let name_for_closure = server_name.clone();
        let url_for_closure = server_url.clone();
        let requires_password = src.requires_password();

        // For passwordless DAAP servers, bypass the dialog entirely
        // and connect directly.
        if backend_type == "daap" && !requires_password {
            // Save the current selection so we can revert on failure.
            pre_connect_selection.set(selected_pos);
            *pending_connection.borrow_mut() = Some(url_for_closure.clone());

            // Mark as connecting → spinner in sidebar.
            if let Some(src) = sidebar_store
                .item(selected_pos)
                .and_downcast_ref::<SourceObject>()
            {
                src.set_connecting(true);
                let src = src.clone();
                sidebar_store.remove(selected_pos);
                sidebar_store.insert(selected_pos, &src);
            }

            let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
            let sidebar_store_for_fail = sidebar_store.clone();
            let sel_for_fail = sel.clone();
            let pre_sel = pre_connect_selection.get();
            let pending_for_fail = pending_connection.clone();
            glib::MainContext::default().spawn_local(async move {
                if fail_rx.recv().await.is_ok() {
                    if let Some(src) = sidebar_store_for_fail
                        .item(selected_pos)
                        .and_downcast_ref::<SourceObject>()
                    {
                        src.set_connecting(false);
                        let src = src.clone();
                        sidebar_store_for_fail.remove(selected_pos);
                        sidebar_store_for_fail.insert(selected_pos, &src);
                    }
                    // Revert sidebar to the previous selection.
                    sel_for_fail.set_selected(pre_sel);
                    // Clear the pending connection guard.
                    *pending_for_fail.borrow_mut() = None;
                }
            });

            let engine_tx = engine_tx.clone();
            let server_url = url_for_closure.clone();
            let server_name = name_for_closure.clone();
            rt_handle.spawn(async move {
                info!(server = %server_url, "Connecting to passwordless DAAP server...");
                match crate::daap::DaapBackend::connect(&server_name, &server_url, None).await {
                    Ok(backend) => {
                        let tracks = backend.all_tracks().await;
                        info!(count = tracks.len(), "DAAP library fetched (no password)");
                        let _ = engine_tx
                            .send(LibraryEvent::RemoteSync {
                                source_key: server_url,
                                tracks,
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "DAAP connection failed");
                        let _ = engine_tx
                            .send(LibraryEvent::Error(format!("DAAP auth failed: {e}")))
                            .await;
                        let _ = fail_tx.send(()).await;
                    }
                }
            });
            return;
        }

        let password_only = backend_type == "daap";

        // Clone Rc's before moving into the auth dialog closure so
        // the outer `Fn` closure can be called multiple times.
        let pre_connect_for_auth = pre_connect_selection.clone();
        let pending_for_auth = pending_connection.clone();
        let sel_for_auth = sel.clone();

        show_auth_dialog(
            &win,
            &server_name,
            &server_url,
            password_only,
            move |user, pass| {
                let engine_tx = engine_tx.clone();
                let server_url = url_for_closure.clone();
                let server_name = name_for_closure.clone();
                let backend_type = backend_type.clone();

                // Mark as connecting → spinner in sidebar.
                if let Some(src) = sidebar_store
                    .item(selected_pos)
                    .and_downcast_ref::<SourceObject>()
                {
                    src.set_connecting(true);
                    let src = src.clone();
                    sidebar_store.remove(selected_pos);
                    sidebar_store.insert(selected_pos, &src);
                }
                // Save the current selection so we can revert on failure.
                pre_connect_for_auth.set(selected_pos);
                *pending_for_auth.borrow_mut() = Some(server_url.clone());

                // One-shot to signal failure back to the main thread so we
                // can clear the spinner (GObjects are not Send).
                let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
                let sidebar_store_for_fail = sidebar_store.clone();
                let sel_for_fail = sel_for_auth.clone();
                let pre_sel = pre_connect_for_auth.get();
                let pending_for_fail = pending_for_auth.clone();
                glib::MainContext::default().spawn_local(async move {
                    if fail_rx.recv().await.is_ok() {
                        if let Some(src) = sidebar_store_for_fail
                            .item(selected_pos)
                            .and_downcast_ref::<SourceObject>()
                        {
                            src.set_connecting(false);
                            let src = src.clone();
                            sidebar_store_for_fail.remove(selected_pos);
                            sidebar_store_for_fail.insert(selected_pos, &src);
                        }
                        // Revert sidebar to the previous selection.
                        sel_for_fail.set_selected(pre_sel);
                        // Clear the pending connection guard.
                        *pending_for_fail.borrow_mut() = None;
                    }
                });

                rt_handle.spawn(async move {
                    let result: Result<
                        Vec<crate::architecture::models::Track>,
                        crate::architecture::error::BackendError,
                    > = match backend_type.as_str() {
                        "jellyfin" => {
                            info!(server = %server_url, "Authenticating with Jellyfin...");
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
                            info!(server = %server_url, "Authenticating with Plex...");
                            match crate::plex::client::PlexClient::authenticate(
                                &server_url,
                                &user,
                                &pass,
                            )
                            .await
                            {
                                Ok(client) => {
                                    match crate::plex::PlexBackend::from_client(
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
                        "daap" => {
                            info!(server = %server_url, "Connecting to DAAP server...");
                            let password = if pass.is_empty() {
                                None
                            } else {
                                Some(pass.as_str())
                            };
                            match crate::daap::DaapBackend::connect(
                                &server_name,
                                &server_url,
                                password,
                            )
                            .await
                            {
                                Ok(backend) => Ok(backend.all_tracks().await),
                                Err(e) => Err(e),
                            }
                        }
                        _ => {
                            // Default: Subsonic
                            info!(server = %server_url, "Authenticating with Subsonic...");
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
                                "Remote library fetched"
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
                                "Authentication failed"
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
            },
        );
    });
}
