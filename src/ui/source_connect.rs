//! Sidebar selection-changed handler — source switching + auth dialogs.
//!
//! Handles clicking sidebar items: switching between local, playlist,
//! USB, radio, connected remote, and unauthenticated remote sources.

use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::local::engine::LibraryEvent;

use super::objects::{SourceObject, TrackObject};
use super::playback::refresh_projected_library_uris;
use super::preferences;
use super::radio::{
    apply_radio_columns, handle_radio_nearme, is_radio_backend, radio_station_to_track_object,
};
use super::server_dialogs::{show_auth_dialog, validate_remote_server_url};
use super::source_navigation::{
    CompletionDisposition, PendingConnection, SourceNavigation, SourceRequest,
};
use super::tracklist;
use super::window::{arch_track_to_object, display_tracks};
use super::window_state::WindowState;

enum RemoteLibrarySnapshot {
    Standard {
        tracks: Vec<crate::architecture::models::Track>,
        generation: u64,
        lease_key: uuid::Uuid,
    },
    Daap {
        tracks: Vec<crate::architecture::models::Track>,
        generation: u64,
        session_key: uuid::Uuid,
    },
}

enum PlaylistLoadOutcome {
    Loaded(Vec<crate::architecture::models::Track>),
    Missing,
    Failed,
}

/// Bound parsed USB rows so a fast filesystem cannot grow memory without
/// limit while GTK is busy. Closing the receiver wakes a blocked producer.
const USB_SCAN_CHANNEL_CAPACITY: usize = 64;

/// Poll exact navigation ownership even while no scan row is arriving. This
/// lets unplug/supersession close the bounded channel promptly; the filesystem
/// walk itself can still remain blocked in the kernel until that call returns.
const USB_SCAN_CANCELLATION_POLL: Duration = Duration::from_millis(50);

fn resolve_source_key(explicit_key: &str, server_url: &str, backend_type: &str) -> String {
    if !explicit_key.is_empty() {
        explicit_key.to_string()
    } else if !server_url.is_empty() {
        server_url.to_string()
    } else if backend_type == "local" || backend_type.is_empty() {
        "local".to_string()
    } else {
        backend_type.to_string()
    }
}

fn restore_pending_selection(
    sidebar_store: &gtk::gio::ListStore,
    selection: &gtk::SingleSelection,
    pending_source_key: &str,
    fallback_position: u32,
) {
    let position = (0..sidebar_store.n_items())
        .find(|position| {
            sidebar_store
                .item(*position)
                .and_downcast_ref::<SourceObject>()
                .is_some_and(|source| source.server_url() == pending_source_key)
        })
        .unwrap_or(fallback_position);
    if selection.selected() != position {
        selection.set_selected(position);
    }
}

/// Cache a completed source load if it is still the newest request for that
/// source, and report whether it also owns the visible projection.
pub(super) fn cache_source_completion(
    navigation: &Rc<RefCell<SourceNavigation>>,
    request: &SourceRequest,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    objects: &[TrackObject],
    active_source_key: &Rc<RefCell<String>>,
) -> bool {
    match navigation.borrow().completion(request) {
        CompletionDisposition::Ignore => false,
        CompletionDisposition::CacheOnly => {
            source_tracks
                .borrow_mut()
                .insert(request.source_key().to_string(), objects.to_vec());
            false
        }
        CompletionDisposition::CacheAndRender => {
            source_tracks
                .borrow_mut()
                .insert(request.source_key().to_string(), objects.to_vec());
            *active_source_key.borrow() == request.source_key()
        }
    }
}

fn evict_source_completion(
    navigation: &Rc<RefCell<SourceNavigation>>,
    request: &SourceRequest,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: &Rc<RefCell<String>>,
) -> bool {
    match navigation.borrow().completion(request) {
        CompletionDisposition::Ignore => false,
        CompletionDisposition::CacheOnly => {
            source_tracks.borrow_mut().remove(request.source_key());
            false
        }
        CompletionDisposition::CacheAndRender => {
            source_tracks.borrow_mut().remove(request.source_key());
            *active_source_key.borrow() == request.source_key()
        }
    }
}

/// Load and publish a playlist through the same generation-owned path used by
/// both explicit navigation and post-reconciliation refreshes.
#[allow(clippy::too_many_arguments)]
pub(super) fn load_playlist_source(
    rt_handle: tokio::runtime::Handle,
    playlist_id: String,
    request: SourceRequest,
    navigation: Rc<RefCell<SourceNavigation>>,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    track_store: gtk::gio::ListStore,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: gtk::Box,
    browser_state: super::browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
) {
    let (tracks_tx, tracks_rx) = async_channel::bounded::<PlaylistLoadOutcome>(1);

    rt_handle.spawn(async move {
        let outcome = match crate::db::connection::init_db().await {
            Ok(db) => {
                let manager = crate::local::playlist_manager::PlaylistManager::new(db);
                match manager.get_playlist(&playlist_id).await {
                    Ok(Some(playlist)) => {
                        let models = if playlist.is_smart {
                            manager.evaluate_smart_playlist(&playlist_id).await
                        } else {
                            manager.get_playlist_tracks(&playlist_id).await
                        };
                        match models {
                            Ok(models) => PlaylistLoadOutcome::Loaded(
                                models
                                    .iter()
                                    .map(crate::local::engine::db_model_to_track)
                                    .collect(),
                            ),
                            Err(error) => {
                                tracing::warn!(%error, "Failed to load playlist tracks");
                                PlaylistLoadOutcome::Failed
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("Playlist disappeared before its tracks were loaded");
                        PlaylistLoadOutcome::Missing
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Failed to identify playlist type");
                        PlaylistLoadOutcome::Failed
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "Failed to open DB for playlist");
                PlaylistLoadOutcome::Failed
            }
        };
        let _ = tracks_tx.send(outcome).await;
    });

    glib::MainContext::default().spawn_local(async move {
        let Ok(outcome) = tracks_rx.recv().await else {
            return;
        };
        let tracks = match outcome {
            PlaylistLoadOutcome::Loaded(tracks) => tracks,
            PlaylistLoadOutcome::Missing => {
                if evict_source_completion(
                    &navigation,
                    &request,
                    &source_tracks,
                    &active_source_key,
                ) {
                    display_tracks(
                        &[],
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }
                return;
            }
            PlaylistLoadOutcome::Failed => {
                // A transient database/query failure is not an empty playlist.
                // Preserve the last successful cache and visible stale-while-
                // revalidate snapshot for a later retry.
                return;
            }
        };
        let objects: Vec<TrackObject> = tracks.iter().map(arch_track_to_object).collect();

        // A paired rename may commit while this database result is in flight.
        // Overlay the latest committed local paths at the GTK publication
        // boundary so either callback order converges on the live URI.
        if let Some(local_tracks) = source_tracks.borrow().get("local") {
            refresh_projected_library_uris(&objects, local_tracks);
        }

        let should_render = cache_source_completion(
            &navigation,
            &request,
            &source_tracks,
            &objects,
            &active_source_key,
        );
        if should_render {
            display_tracks(
                &objects,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
        } else {
            tracing::debug!(
                source = %request.source_key(),
                generation = request.generation(),
                "Playlist result cached or ignored without rendering"
            );
        }
    });
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
    let source_navigation = state.source_navigation.clone();
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
            let pending = pending_connection.borrow().clone();
            if let Some(pending) = pending {
                // If clicking the same server that's already connecting, just ignore.
                if src.server_url() == pending.source_key() {
                    return;
                }
                // If clicking a different server while one is connecting,
                // also ignore — let the first connection finish first.
                if src.connecting() || (!src.connected() && !src.server_url().is_empty()) {
                    restore_pending_selection(
                        &sidebar_store,
                        sel,
                        pending.source_key(),
                        pre_connect_selection.get(),
                    );
                    return;
                }
            }
        }

        let backend_type = src.backend_type();

        // Determine the source key. Device rows carry an explicit logical key;
        // legacy/remote rows fall back to their server URL, while local static
        // sources use the backend type so they do not collapse into "local".
        let explicit_key = src.source_key();
        let url = src.server_url();
        let key = resolve_source_key(&explicit_key, &url, &backend_type);

        // ── Local source: switch to local view ───────────────────
        if key == "local" {
            source_navigation.borrow_mut().select("local");
            *active_source_key.borrow_mut() = "local".to_string();
            pre_connect_selection.set(sel.selected());

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
            let request = source_navigation
                .borrow_mut()
                .select(playlist_source_key.clone());
            *active_source_key.borrow_mut() = playlist_source_key.clone();
            pre_connect_selection.set(sel.selected());

            // Restore music column layout (not radio).
            apply_radio_columns(&column_view, false);
            // Restore column + browser visibility from config (issue #38:
            // opening a playlist must not reset hidden columns to default).
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let cached = source_tracks.borrow().get(&playlist_source_key).cloned();
            display_tracks(
                cached.as_deref().unwrap_or_default(),
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );

            load_playlist_source(
                rt_handle.clone(),
                playlist_id,
                request,
                source_navigation.clone(),
                source_tracks.clone(),
                active_source_key.clone(),
                track_store.clone(),
                master_tracks.clone(),
                browser_widget.clone(),
                browser_state.clone(),
                status_label.clone(),
                column_view.clone(),
            );
            return;
        }

        // ── USB device source: scan and display music files ──────
        if backend_type == "usb-device" {
            let Some(mount_point) = src.device_mount_point() else {
                return;
            };

            let request = source_navigation.borrow_mut().select(key.clone());
            *active_source_key.borrow_mut() = key.clone();
            pre_connect_selection.set(sel.selected());

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
                // The source identity changes immediately, so its projection
                // must change with it. Leaving the previous source's rows in
                // place would let a click queue those tracks under this USB
                // source key while a large device scan is still running.
                display_tracks(
                    &[],
                    &track_store,
                    &master_tracks,
                    &browser_widget,
                    &browser_state,
                    &status_label,
                    &column_view,
                );

                // Scan on a background thread to avoid blocking UI.
                let mount = mount_point;
                let track_store = track_store.clone();
                let master_tracks = master_tracks.clone();
                let source_tracks = source_tracks.clone();
                let browser_widget = browser_widget.clone();
                let browser_state = browser_state.clone();
                let status_label = status_label.clone();
                let column_view = column_view.clone();
                let active_source_key = active_source_key.clone();
                let source_navigation = source_navigation.clone();
                let request = request.clone();

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
                let (scan_tx, scan_rx) =
                    async_channel::bounded::<ScanRow>(USB_SCAN_CHANNEL_CAPACITY);

                // Background thread: stream the device filesystem into a
                // bounded channel. A superseded/unplugged receiver closes the
                // channel, waking send_blocking and stopping the producer.
                if let Err(error) = std::thread::Builder::new()
                    .name("usb-scan".to_string())
                    .spawn(move || {
                        for path in enumerate_device_audio_files(&mount) {
                            if scan_tx.is_closed() {
                                break;
                            }
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
                                if scan_tx.send_blocking(row).is_err() {
                                    break;
                                }
                            }
                        }
                    })
                {
                    tracing::warn!(%error, "Failed to start USB device scan worker");
                    // The failed spawn drops its closure and sender, so the
                    // channel is closed explicitly and this request remains
                    // uncached, allowing a later selection to retry.
                    scan_rx.close();
                    return;
                }

                // Collect results on the GTK main thread.
                glib::MainContext::default().spawn_local(async move {
                    let mut objects = Vec::new();
                    loop {
                        if source_navigation.borrow().completion(&request)
                            == CompletionDisposition::Ignore
                        {
                            scan_rx.close();
                            return;
                        }

                        let receive = scan_rx.recv();
                        futures::pin_mut!(receive);
                        let cancellation_poll = glib::timeout_future(USB_SCAN_CANCELLATION_POLL);
                        match futures::future::select(receive, cancellation_poll).await {
                            futures::future::Either::Left((Ok(row), _)) => {
                                // Ownership may have changed while recv was
                                // ready but before GTK resumed this task.
                                if source_navigation.borrow().completion(&request)
                                    == CompletionDisposition::Ignore
                                {
                                    scan_rx.close();
                                    return;
                                }
                                let obj = TrackObject::new(
                                    row.0, &row.1, row.2, &row.3, &row.4, &row.5, &row.6, row.7,
                                    &row.8, row.9, row.10, 0, &row.11, &row.12,
                                );
                                objects.push(obj);
                            }
                            futures::future::Either::Left((Err(_), _)) => break,
                            futures::future::Either::Right(((), _)) => {}
                        }
                    }

                    if source_navigation.borrow().completion(&request)
                        == CompletionDisposition::Ignore
                    {
                        scan_rx.close();
                        return;
                    }

                    let should_render = cache_source_completion(
                        &source_navigation,
                        &request,
                        &source_tracks,
                        &objects,
                        &active_source_key,
                    );

                    if should_render {
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
            let request = source_navigation.borrow_mut().select(backend_type.clone());
            *active_source_key.borrow_mut() = backend_type.clone();
            pre_connect_selection.set(sel.selected());

            // Switch to radio column layout.
            apply_radio_columns(&column_view, true);
            // Hide browser for radio.
            browser_widget.set_visible(false);

            // Show a prior completed result while refreshing. If this source
            // has never loaded, clear the old source immediately.
            if let Some(cached) = source_tracks.borrow().get(&backend_type).cloned() {
                display_tracks(
                    &cached,
                    &track_store,
                    &master_tracks,
                    &browser_widget,
                    &browser_state,
                    &status_label,
                    &column_view,
                );
            } else {
                track_store.remove_all();
                tracklist::update_status(&status_label, &[]);
                *master_tracks.borrow_mut() = Vec::new();
            }

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
                let source_navigation = source_navigation.clone();
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
                    source_navigation,
                    request,
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
                let source_navigation = source_navigation.clone();
                let source_tracks = source_tracks.clone();

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
                        let stations: Vec<crate::radio::RadioStation> =
                            serde_json::from_str(&json).unwrap_or_default();
                        let objects: Vec<TrackObject> =
                            stations.iter().map(radio_station_to_track_object).collect();
                        if cache_source_completion(
                            &source_navigation,
                            &request,
                            &source_tracks,
                            &objects,
                            &active_source_key,
                        ) {
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
                    }
                });
            }
            return;
        }

        // ── Connected source: switch view ───────────────────────
        if src.connected() {
            source_navigation.borrow_mut().select(key.clone());
            *active_source_key.borrow_mut() = key.clone();
            pre_connect_selection.set(sel.selected());

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

        // Saved rows can predate the current URL policy. Reject an unsafe
        // standard-backend URL before it is displayed in an auth dialog,
        // logged by a connection task, or registered as source ownership.
        // The fixed error text cannot echo user-info or query credentials from
        // the rejected row.
        if let Err(message) = validate_remote_server_url(&server_url) {
            tracing::warn!(error = message, "Remote server URL rejected");
            let _ = engine_tx.try_send(LibraryEvent::Error(message.to_string()));
            return;
        }

        // Backend/session generations protect remote ownership, but they do
        // not say whether the user still wants this pending connection to
        // change the selected source when it completes.
        let connection_request = source_navigation.borrow_mut().select(server_url.clone());

        // For passwordless DAAP servers, bypass the dialog entirely
        // and connect directly.
        if backend_type == "daap" && !requires_password {
            *pending_connection.borrow_mut() = Some(PendingConnection::new(
                url_for_closure.clone(),
                connection_request.clone(),
            ));

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
            let navigation_for_fail = source_navigation.clone();
            let request_for_fail = connection_request.clone();
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
                    let owns_pending = pending_for_fail
                        .borrow()
                        .as_ref()
                        .is_some_and(|pending| pending.request() == &request_for_fail);
                    if owns_pending {
                        *pending_for_fail.borrow_mut() = None;
                        if navigation_for_fail.borrow().is_current(&request_for_fail) {
                            sel_for_fail.set_selected(pre_sel);
                        }
                    }
                }
            });

            let sidebar_store_for_auth = sidebar_store.clone();
            let sel_for_auth = sel.clone();
            let pending_for_auth = pending_connection.clone();
            let navigation_for_auth = source_navigation.clone();
            let request_for_auth = connection_request.clone();
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
                    let owns_pending = pending_for_auth
                        .borrow()
                        .as_ref()
                        .is_some_and(|pending| pending.request() == &request_for_auth);
                    if owns_pending {
                        *pending_for_auth.borrow_mut() = None;
                        if navigation_for_auth.borrow().is_current(&request_for_auth) {
                            // Setting the same position is a no-op in
                            // GtkSingleSelection, so deselect then reselect.
                            sel_for_auth.set_selected(gtk::INVALID_LIST_POSITION);
                            sel_for_auth.set_selected(selected_pos);
                        }
                    }
                }
            });

            let engine_tx = engine_tx.clone();
            let server_url = url_for_closure.clone();
            let server_name = name_for_closure.clone();
            rt_handle.spawn(async move {
                info!("Connecting to passwordless DAAP server...");
                let Some(attempt) = crate::daap::begin_connect(server_url.clone()) else {
                    tracing::debug!("Skipping DAAP connect during shutdown");
                    return;
                };
                match crate::daap::DaapBackend::connect(&server_name, &server_url, None).await {
                    Ok(backend) => {
                        let Some(session) = attempt.retain(backend).await else {
                            tracing::debug!("DAAP connect was superseded");
                            return;
                        };
                        let tracks = session.all_tracks().await;
                        if !session.is_current() {
                            tracing::debug!("DAAP sync was superseded");
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
                            tracing::debug!("Ignoring superseded DAAP authentication failure");
                            return;
                        }
                        info!("DAAP server requires a password — re-prompting via auth dialog");
                        let _ = auth_needed_tx.send(()).await;
                    }
                    Err(e) => {
                        if !attempt.is_latest() {
                            tracing::debug!("Ignoring superseded DAAP connection failure");
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
        *pending_connection.borrow_mut() = Some(PendingConnection::new(
            url_for_closure.clone(),
            connection_request.clone(),
        ));

        // Clone Rc's before moving into the auth dialog closure so
        // the outer `Fn` closure can be called multiple times.
        let pre_connect_for_auth = pre_connect_selection.clone();
        let pending_for_auth = pending_connection.clone();
        let sel_for_auth = sel.clone();

        // If the user cancels / escapes the auth dialog, drop the
        // pending-connection guard so the next sidebar click works.
        let pending_for_cancel = pending_connection.clone();
        let navigation_for_cancel = source_navigation.clone();
        let request_for_cancel = connection_request.clone();
        let sel_for_cancel = sel.clone();
        let pre_sel_for_cancel = pre_connect_selection.get();
        let on_cancel = move || {
            let owns_pending = pending_for_cancel
                .borrow()
                .as_ref()
                .is_some_and(|pending| pending.request() == &request_for_cancel);
            if owns_pending {
                *pending_for_cancel.borrow_mut() = None;
                if navigation_for_cancel
                    .borrow()
                    .is_current(&request_for_cancel)
                {
                    sel_for_cancel.set_selected(pre_sel_for_cancel);
                }
            }
        };
        let navigation_for_submit = source_navigation.clone();

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
                let connection_request = connection_request.clone();

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
                *pending_for_auth.borrow_mut() = Some(PendingConnection::new(
                    server_url.clone(),
                    connection_request.clone(),
                ));

                // One-shot to signal failure back to the main thread so we
                // can clear the spinner (GObjects are not Send).
                let (fail_tx, fail_rx) = async_channel::bounded::<()>(1);
                let sidebar_store_for_fail = sidebar_store.clone();
                let sel_for_fail = sel_for_auth.clone();
                let pre_sel = pre_connect_for_auth.get();
                let pending_for_fail = pending_for_auth.clone();
                let navigation_for_fail = navigation_for_submit.clone();
                let request_for_fail = connection_request.clone();
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
                        let owns_pending = pending_for_fail
                            .borrow()
                            .as_ref()
                            .is_some_and(|pending| pending.request() == &request_for_fail);
                        if owns_pending {
                            *pending_for_fail.borrow_mut() = None;
                            if navigation_for_fail.borrow().is_current(&request_for_fail) {
                                sel_for_fail.set_selected(pre_sel);
                            }
                        }
                    }
                });

                rt_handle.spawn(async move {
                    let result: Result<
                        Option<RemoteLibrarySnapshot>,
                        crate::architecture::error::BackendError,
                    > = match backend_type.as_str() {
                        "jellyfin" => {
                            info!("Authenticating with Jellyfin...");
                            let Some(attempt) =
                                crate::source_registry::begin_connect(server_url.clone())
                            else {
                                return;
                            };
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
                                            Ok(attempt.retain(Arc::new(backend)).and_then(
                                                |source| {
                                                    source.is_current().then(|| {
                                                        RemoteLibrarySnapshot::Standard {
                                                            tracks,
                                                            generation: source.generation(),
                                                            lease_key: source.lease_key(),
                                                        }
                                                    })
                                                },
                                            ))
                                        }
                                        Err(_) if !attempt.is_latest() => Ok(None),
                                        Err(e) => Err(e),
                                    }
                                }
                                Err(_) if !attempt.is_latest() => Ok(None),
                                Err(e) => Err(e),
                            }
                        }
                        "plex" => {
                            info!("Authenticating with Plex...");
                            let Some(attempt) =
                                crate::source_registry::begin_connect(server_url.clone())
                            else {
                                return;
                            };
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
                                        Ok(backend) => {
                                            let tracks = backend.all_tracks().await;
                                            Ok(attempt.retain(Arc::new(backend)).and_then(
                                                |source| {
                                                    source.is_current().then(|| {
                                                        RemoteLibrarySnapshot::Standard {
                                                            tracks,
                                                            generation: source.generation(),
                                                            lease_key: source.lease_key(),
                                                        }
                                                    })
                                                },
                                            ))
                                        }
                                        Err(_) if !attempt.is_latest() => Ok(None),
                                        Err(e) => Err(e),
                                    }
                                }
                                Err(_) if !attempt.is_latest() => Ok(None),
                                Err(e) => Err(e),
                            }
                        }
                        "daap" => {
                            info!("Connecting to DAAP server...");
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
                                },
                            }
                        }
                        _ => {
                            // Default: Subsonic
                            info!("Authenticating with Subsonic...");
                            let Some(attempt) =
                                crate::source_registry::begin_connect(server_url.clone())
                            else {
                                return;
                            };
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
                                    Ok(attempt.retain(Arc::new(backend)).and_then(|source| {
                                        source.is_current().then(|| {
                                            RemoteLibrarySnapshot::Standard {
                                                tracks,
                                                generation: source.generation(),
                                                lease_key: source.lease_key(),
                                            }
                                        })
                                    }))
                                }
                                Err(_) if !attempt.is_latest() => Ok(None),
                                Err(e) => Err(e),
                            }
                        }
                    };

                    match result {
                        Ok(Some(snapshot)) => {
                            let (count, event) = match snapshot {
                                RemoteLibrarySnapshot::Standard {
                                    tracks,
                                    generation,
                                    lease_key,
                                } => {
                                    let count = tracks.len();
                                    (
                                        count,
                                        LibraryEvent::RemoteSync {
                                            source_key: server_url,
                                            generation,
                                            lease_key,
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
fn enumerate_device_audio_files(
    mount: &std::path::Path,
) -> impl Iterator<Item = std::path::PathBuf> {
    walkdir::WalkDir::new(mount)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| crate::local::tag_parser::is_audio_file(path))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{enumerate_device_audio_files, resolve_source_key};

    #[test]
    fn explicit_source_identity_precedes_legacy_fallbacks() {
        assert_eq!(
            resolve_source_key("device:uuid:123", "file:///legacy", "usb-device"),
            "device:uuid:123"
        );
        assert_eq!(
            resolve_source_key("", "https://music.example.test", "subsonic"),
            "https://music.example.test"
        );
        assert_eq!(resolve_source_key("", "", "radio-topvote"), "radio-topvote");
        assert_eq!(resolve_source_key("", "", "local"), "local");
        assert_eq!(resolve_source_key("", "", ""), "local");
    }

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

        let found: Vec<_> = enumerate_device_audio_files(device.path()).collect();
        assert_eq!(file_names(&found), vec!["root.mp3", "track.flac"]);
    }

    #[test]
    fn device_audio_enumeration_is_lazy() {
        let device = TestTree::new("lazy");
        let track = device.audio("removed-before-poll.mp3");
        let mut paths = enumerate_device_audio_files(device.path());

        std::fs::remove_file(track).expect("remove track before polling iterator");

        assert_eq!(paths.next(), None);
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

        let found: Vec<_> = enumerate_device_audio_files(device.path()).collect();

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
