//! Sidebar selection-changed handler — source switching + auth dialogs.
//!
//! Handles clicking sidebar items: switching between local, playlist,
//! USB, radio, connected remote, and unauthenticated remote sources.

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use crate::local::engine::LibraryEvent;

use super::objects::{SourceObject, TrackObject};
use super::playback::refresh_projected_library_uris;
use super::preferences;
use super::radio::{
    apply_radio_columns, handle_radio_nearme, is_radio_backend, radio_station_to_track_object,
};
use super::server_dialogs::show_auth_dialog;
use super::tracklist;
use super::window::{arch_track_to_object, display_tracks};
use super::window_state::WindowState;

enum RemoteLibrarySnapshot {
    Standard(Vec<crate::architecture::models::Track>),
    Daap {
        tracks: Vec<crate::architecture::models::Track>,
        generation: u64,
        session_key: uuid::Uuid,
    },
}

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
            return;
        }

        // ── Playlist source: fetch playlist tracks ───────────────
        if backend_type == "playlist" || backend_type == "smart-playlist" {
            let playlist_id = src.playlist_id();
            if playlist_id.is_empty() {
                return;
            }

            let playlist_source_key =
                format!("{}{playlist_id}", super::playback::PLAYLIST_SOURCE_PREFIX);
            *active_source_key.borrow_mut() = playlist_source_key.clone();

            // Restore music column layout (not radio).
            apply_radio_columns(&column_view, false);
            // Restore column + browser visibility from config (issue #38:
            // opening a playlist must not reset hidden columns to default).
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
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
            let pid = playlist_id.clone();
            let source_tracks_for_load = source_tracks.clone();
            let active_source_key_for_load = active_source_key.clone();
            let requested_source_key = playlist_source_key;

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

                    // A paired rename may commit while this database result is
                    // in flight. Overlay the latest committed local paths at
                    // the GTK publication boundary so either callback order
                    // converges on the live URI.
                    if let Some(local_tracks) = source_tracks_for_load.borrow().get("local") {
                        refresh_projected_library_uris(&objects, local_tracks);
                    }

                    // A slow playlist query must never replace a source the
                    // user selected while it was running.
                    if *active_source_key_for_load.borrow() != requested_source_key {
                        tracing::debug!(
                            source = %requested_source_key,
                            "Ignoring playlist result for an inactive source"
                        );
                        return;
                    }
                    display_tracks(
                        &objects,
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
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
                    for path in enumerate_device_audio_files(mount_path) {
                        let path = path.as_path();
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
                                parsed.composer.unwrap_or_default(),
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
                            row.0, &row.1, row.2, &row.3, &row.4, &row.5, &row.6, row.7, &row.8,
                            row.9, row.10, 0, &row.11, &row.12,
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

            // Handle "Stations Near Me" with geo consent.
            if backend_type == super::radio::NEARME_SOURCE_KEY {
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
                let active_source_key = active_source_key.clone();
                let requested_source_key = backend_type.clone();

                let (stations_tx, stations_rx) = async_channel::bounded::<String>(1);

                rt_handle.spawn(async move {
                    let stations = match crate::radio::RadioBrowserClient::new() {
                        Ok(client) if bt == "radio-topclick" => client.fetch_top_click(None).await,
                        Ok(client) => client.fetch_top_vote(None).await,
                        Err(_) => {
                            tracing::error!("Could not build the Radio-Browser HTTP client");
                            Vec::new()
                        }
                    };
                    let json = serde_json::to_string(&stations).unwrap_or_default();
                    let _ = stations_tx.send(json).await;
                });

                glib::MainContext::default().spawn_local(async move {
                    if let Ok(json) = stations_rx.recv().await {
                        // The fetch takes seconds. If the user has since picked
                        // another source, this result is stale and must not
                        // overwrite whatever they are looking at now.
                        if *active_source_key.borrow() != requested_source_key {
                            return;
                        }
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
            // Restore column + browser visibility from config (issue #38:
            // connecting a remote source must not reset hidden columns to
            // default).
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
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
            // Separate signal for "server actually does require a password":
            // the no-password connect came back with AuthenticationFailed, so
            // we flip the flag and re-fire the selection to let the existing
            // auth-dialog branch handle it.
            let (auth_needed_tx, auth_needed_rx) = async_channel::bounded::<()>(1);
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

            let sidebar_store_for_auth = sidebar_store.clone();
            let sel_for_auth = sel.clone();
            let pending_for_auth = pending_connection.clone();
            glib::MainContext::default().spawn_local(async move {
                if auth_needed_rx.recv().await.is_ok() {
                    // Clear connecting state and flip requires_password so the
                    // re-fired selection lands in the auth-dialog branch.
                    if let Some(src) = sidebar_store_for_auth
                        .item(selected_pos)
                        .and_downcast_ref::<SourceObject>()
                    {
                        src.set_connecting(false);
                        src.set_requires_password(true);
                        let src = src.clone();
                        sidebar_store_for_auth.remove(selected_pos);
                        sidebar_store_for_auth.insert(selected_pos, &src);
                    }
                    // Drop the pending guard and re-fire selection-changed.
                    // Setting the same position is a no-op in GtkSingleSelection,
                    // so deselect (INVALID_LIST_POSITION) then reselect.
                    *pending_for_auth.borrow_mut() = None;
                    sel_for_auth.set_selected(gtk::INVALID_LIST_POSITION);
                    sel_for_auth.set_selected(selected_pos);
                }
            });

            let engine_tx = engine_tx.clone();
            let server_url = url_for_closure.clone();
            let server_name = name_for_closure.clone();
            rt_handle.spawn(async move {
                info!(server = %server_url, "Connecting to passwordless DAAP server...");
                let Some(attempt) = crate::daap::begin_connect(server_url.clone()) else {
                    tracing::debug!(server = %server_url, "Skipping DAAP connect during shutdown");
                    return;
                };
                match crate::daap::DaapBackend::connect(&server_name, &server_url, None).await {
                    Ok(backend) => {
                        let Some(session) = attempt.retain(backend).await else {
                            tracing::debug!(server = %server_url, "DAAP connect was superseded");
                            return;
                        };
                        let tracks = session.all_tracks().await;
                        if !session.is_current() {
                            tracing::debug!(server = %server_url, "DAAP sync was superseded");
                            return;
                        }
                        info!(count = tracks.len(), "DAAP library fetched (no password)");
                        let _ = engine_tx
                            .send(LibraryEvent::DaapSync {
                                source_key: server_url,
                                generation: session.generation(),
                                session_key: session.session_key(),
                                tracks,
                            })
                            .await;
                    }
                    Err(crate::architecture::error::BackendError::AuthenticationFailed {
                        ..
                    }) => {
                        if !attempt.is_latest() {
                            tracing::debug!(server = %server_url, "Ignoring superseded DAAP authentication failure");
                            return;
                        }
                        info!(
                            server = %server_url,
                            "DAAP server requires a password — re-prompting via auth dialog"
                        );
                        let _ = auth_needed_tx.send(()).await;
                    }
                    Err(e) => {
                        if !attempt.is_latest() {
                            tracing::debug!(server = %server_url, "Ignoring superseded DAAP connection failure");
                            return;
                        }
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

        // Set the pending-connection guard *before* the auth dialog is
        // shown so a second sidebar click while the dialog is open is
        // ignored at the top of this handler. The submit closure and
        // the cancel callback below clear it when appropriate.
        *pending_connection.borrow_mut() = Some(url_for_closure.clone());
        pre_connect_selection.set(selected_pos);

        // Clone Rc's before moving into the auth dialog closure so
        // the outer `Fn` closure can be called multiple times.
        let pre_connect_for_auth = pre_connect_selection.clone();
        let pending_for_auth = pending_connection.clone();
        let sel_for_auth = sel.clone();

        // If the user cancels / escapes the auth dialog, drop the
        // pending-connection guard so the next sidebar click works.
        let pending_for_cancel = pending_connection.clone();
        let on_cancel = move || {
            *pending_for_cancel.borrow_mut() = None;
        };

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
                        Option<RemoteLibrarySnapshot>,
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
                                        Ok(backend) => Ok(Some(RemoteLibrarySnapshot::Standard(
                                            backend.all_tracks().await,
                                        ))),
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
                                        Ok(backend) => Ok(Some(RemoteLibrarySnapshot::Standard(
                                            backend.all_tracks().await,
                                        ))),
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
                            match crate::daap::begin_connect(server_url.clone()) {
                                None => Ok(None),
                                Some(attempt) => match crate::daap::DaapBackend::connect(
                                    &server_name,
                                    &server_url,
                                    password,
                                )
                                .await
                                {
                                    Ok(backend) => match attempt.retain(backend).await {
                                        Some(session) => {
                                            let tracks = session.all_tracks().await;
                                            if session.is_current() {
                                                Ok(Some(RemoteLibrarySnapshot::Daap {
                                                    tracks,
                                                    generation: session.generation(),
                                                    session_key: session.session_key(),
                                                }))
                                            } else {
                                                Ok(None)
                                            }
                                        }
                                        None => Ok(None),
                                    },
                                    Err(_) if !attempt.is_latest() => Ok(None),
                                    Err(error) => Err(error),
                                }
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
                                Ok(backend) => Ok(Some(RemoteLibrarySnapshot::Standard(
                                    backend.all_tracks().await,
                                ))),
                                Err(e) => Err(e),
                            }
                        }
                    };

                    match result {
                        Ok(Some(snapshot)) => {
                            let (count, event) = match snapshot {
                                RemoteLibrarySnapshot::Standard(tracks) => {
                                    let count = tracks.len();
                                    (
                                        count,
                                        LibraryEvent::RemoteSync {
                                            source_key: server_url,
                                            tracks,
                                        },
                                    )
                                }
                                RemoteLibrarySnapshot::Daap {
                                    tracks,
                                    generation,
                                    session_key,
                                } => {
                                    let count = tracks.len();
                                    (
                                        count,
                                        LibraryEvent::DaapSync {
                                            source_key: server_url,
                                            generation,
                                            session_key,
                                            tracks,
                                        },
                                    )
                                }
                            };
                            info!(
                                backend = %backend_type,
                                count,
                                "Remote library fetched"
                            );
                            let _ = engine_tx.send(event).await;
                        }
                        Ok(None) => tracing::debug!(
                            backend = %backend_type,
                            server = %server_url,
                            "Remote connection was superseded or shutdown-gated"
                        ),
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
            on_cancel,
        );
    });
}

/// Enumerate audio files on a mounted device, never leaving the device.
///
/// Symlinks are not followed. A USB stick containing `music -> /home/user`
/// would otherwise make Tributary walk the user's entire home directory and
/// index whatever it found there as "on the device". `file_type()` is checked
/// rather than `Path::is_file()` because the latter follows the link anyway,
/// which would still pull in an individual file symlinked from off the device.
///
/// This matches the policy the library scanner already applies in
/// `local::engine`.
fn enumerate_device_audio_files(mount: &std::path::Path) -> Vec<std::path::PathBuf> {
    walkdir::WalkDir::new(mount)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| crate::local::tag_parser::is_audio_file(path))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::enumerate_device_audio_files;

    struct TestTree {
        path: PathBuf,
    }

    impl TestTree {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("tributary-device-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create test tree");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        /// Only the symlink test needs a bare directory, and that test is Unix
        /// only — without this gate the helper is dead code on Windows, which
        /// `-D warnings` rejects.
        #[cfg(unix)]
        fn directory(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::create_dir_all(&path).expect("create directory");
            path
        }

        fn audio(&self, relative: &str) -> PathBuf {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, b"audio").expect("write audio file");
            path
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn file_names(paths: &[PathBuf]) -> Vec<String> {
        let mut names: Vec<String> = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_device_scan_finds_audio_in_nested_directories() {
        let device = TestTree::new("nested");
        device.audio("root.mp3");
        device.audio("Album/track.flac");
        device.audio("Album/cover.jpg");
        device.audio("notes.txt");

        let found = enumerate_device_audio_files(device.path());
        assert_eq!(file_names(&found), vec!["root.mp3", "track.flac"]);
    }

    /// The P2.4 defect: the walk followed symlinks, so a stick containing
    /// `music -> /home/user` walked the whole home directory.
    #[cfg(unix)]
    #[test]
    fn a_device_scan_never_follows_a_symlink_off_the_device() {
        let elsewhere = TestTree::new("elsewhere");
        elsewhere.audio("private.mp3");
        elsewhere.audio("Deep/deeper/secret.mp3");

        let device = TestTree::new("device");
        device.audio("on-device.mp3");
        let escape = device.directory("Music").join("escape");
        std::os::unix::fs::symlink(elsewhere.path(), &escape).expect("link a directory off-device");
        std::os::unix::fs::symlink(
            elsewhere.path().join("private.mp3"),
            device.path().join("linked.mp3"),
        )
        .expect("link a file off-device");

        let found = enumerate_device_audio_files(device.path());

        assert_eq!(
            file_names(&found),
            vec!["on-device.mp3"],
            "only files physically on the device may be indexed"
        );
        assert!(
            !found.iter().any(|path| path.starts_with(elsewhere.path())),
            "no result may resolve outside the mount"
        );
    }
}
