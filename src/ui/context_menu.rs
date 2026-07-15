//! Right-click context menu on the tracklist ColumnView.
//!
//! Handles "Remove from Playlist", "Add to Playlist", and "Properties…"
//! actions triggered from right-clicking selected tracks.

use adw::prelude::*;
use sea_orm::{EntityTrait, QueryFilter};

use super::browser;
use super::objects::{SourceObject, TrackObject};
use super::tracklist;
use super::window_state::WindowState;

/// Wire the right-click context menu on the tracklist `ColumnView`.
///
/// Adds a gesture controller that shows a popover menu with actions
/// relevant to the current selection and source context.
pub fn setup_context_menu(state: &WindowState) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3); // right-click
    let sm = state.sort_model.clone();
    let sidebar_store = state.sidebar_store.clone();
    let active_source_key = state.active_source_key.clone();
    let rt_handle = state.rt_handle.clone();
    let track_store = state.track_store.clone();
    let source_tracks = state.source_tracks.clone();
    let source_navigation = state.source_navigation.clone();
    let master_tracks = state.master_tracks.clone();
    let status_label = state.status_label.clone();
    let browser_widget = state.browser_widget.clone();
    let browser_state = state.browser_state.clone();

    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        let Some(widget) = gesture.widget() else {
            return;
        };
        let Ok(cv) = widget.downcast::<gtk::ColumnView>() else {
            return;
        };

        let active_key = active_source_key.borrow().clone();
        let is_playlist_view = active_key.starts_with("playlist:");

        // Collect selected track URIs from the MultiSelection model.
        let selection_model = cv.model();
        let Some(sel) = selection_model.and_then(|m| m.downcast::<gtk::MultiSelection>().ok())
        else {
            return;
        };

        let selected = sel.selection();
        if selected.is_empty() {
            return;
        }

        let menu = gtk::gio::Menu::new();
        let action_group = gtk::gio::SimpleActionGroup::new();

        if is_playlist_view {
            build_remove_from_playlist_action(
                &menu,
                &action_group,
                &active_key,
                &sm,
                &selected,
                &rt_handle,
                &track_store,
                &source_tracks,
                &source_navigation,
                &master_tracks,
                &status_label,
                &browser_widget,
                &browser_state,
            );
        } else {
            build_add_to_playlist_actions(
                &menu,
                &action_group,
                &sidebar_store,
                &sm,
                &selected,
                &rt_handle,
            );
        }

        // ── Properties… ──────────────────────────────────────────
        build_properties_action(&menu, &action_group, gesture, &sm, &selected);

        if menu.n_items() == 0 {
            return;
        }

        cv.insert_action_group("tracklist-ctx", Some(&action_group));

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&cv);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

        // Disable the internal ScrolledWindow that GTK4 PopoverMenu
        // creates — it adds unnecessary scrollbars for small menus.
        disable_popover_scrollbars(&popover);

        popover.popup();
    });

    state.column_view.add_controller(gesture);
}

// ═══════════════════════════════════════════════════════════════════════
// Action builders
// ═══════════════════════════════════════════════════════════════════════

/// Build the "Remove from Playlist" action for playlist views.
#[allow(clippy::too_many_arguments)]
fn build_remove_from_playlist_action(
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    active_key: &str,
    sm: &gtk::SortListModel,
    selected: &gtk::Bitset,
    rt_handle: &tokio::runtime::Handle,
    track_store: &gtk::gio::ListStore,
    source_tracks: &std::rc::Rc<
        std::cell::RefCell<std::collections::HashMap<String, Vec<TrackObject>>>,
    >,
    source_navigation: &std::rc::Rc<std::cell::RefCell<super::source_navigation::SourceNavigation>>,
    master_tracks: &std::rc::Rc<std::cell::RefCell<Vec<TrackObject>>>,
    status_label: &gtk::Label,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
) {
    let playlist_id = active_key
        .strip_prefix("playlist:")
        .unwrap_or("")
        .to_string();
    let rt = rt_handle.clone();
    let track_store = track_store.clone();
    let source_tracks = source_tracks.clone();
    let source_navigation = source_navigation.clone();
    let master_tracks = master_tracks.clone();
    let status_label = status_label.clone();
    let browser_widget = browser_widget.clone();
    let browser_state = browser_state.clone();
    let active_key = active_key.to_string();

    // Collect URIs of selected tracks.
    let selected_uris = collect_selected_uris(sm, selected);

    let remove_action = gtk::gio::SimpleAction::new("remove-from-playlist", None);
    let uris = selected_uris;
    remove_action.connect_activate(move |_, _| {
        let pid = playlist_id.clone();
        let uris = uris.clone();
        let track_store = track_store.clone();
        let source_tracks = source_tracks.clone();
        let source_navigation = source_navigation.clone();
        let master_tracks = master_tracks.clone();
        let status_label = status_label.clone();
        let browser_widget = browser_widget.clone();
        let browser_state = browser_state.clone();
        let active_key = active_key.clone();

        // A popover action can outlive the rows it was built for. It must not
        // mutate another source, and an already-running refresh must not put
        // the just-removed entry back afterward.
        if !source_navigation.borrow().is_key(&active_key) {
            return;
        }
        source_navigation.borrow_mut().select(active_key.clone());

        // Remove from the visible store immediately. Honour the per-URI
        // selection count so that selecting one of N duplicate rows removes
        // exactly one occurrence, not all N.
        let mut remaining = selection_counts(&uris);
        let mut i: u32 = 0;
        while i < track_store.n_items() {
            let uri = track_store
                .item(i)
                .and_downcast::<TrackObject>()
                .map(|t| t.uri());
            let mut remove = false;
            if let Some(u) = uri.as_deref() {
                if let Some(count) = remaining.get_mut(u) {
                    if *count > 0 {
                        *count -= 1;
                        remove = true;
                    }
                }
            }
            if remove {
                track_store.remove(i);
                // Don't advance `i` — the next item shifted down into
                // this slot.
            } else {
                i += 1;
            }
        }

        // Update master + status (same per-URI count limit as the store).
        {
            let mut st = source_tracks.borrow_mut();
            if let Some(tracks) = st.get_mut(&active_key) {
                let mut remaining = selection_counts(&uris);
                tracks.retain(|t| match remaining.get_mut(t.uri().as_str()) {
                    Some(count) if *count > 0 => {
                        *count -= 1;
                        false // remove this occurrence
                    }
                    _ => true, // keep
                });
            }
        }
        let st = source_tracks.borrow();
        let current = st.get(&active_key).cloned().unwrap_or_default();
        *master_tracks.borrow_mut() = current.clone();
        tracklist::update_status(&status_label, &current);
        browser::rebuild_browser_data(&browser_widget, &browser_state, &current);

        // Remove from DB in background.
        rt.spawn(async move {
            match crate::db::connection::init_db().await {
                Ok(db) => {
                    let mgr = crate::local::playlist_manager::PlaylistManager::new(db.clone());
                    // Get all entries for this playlist, match by track file path.
                    if let Ok(entries) = crate::db::entities::playlist_entry::Entity::find()
                        .filter(
                            <crate::db::entities::playlist_entry::Column as sea_orm::ColumnTrait>::eq(
                                &crate::db::entities::playlist_entry::Column::PlaylistId,
                                &pid,
                            ),
                        )
                        .all(&db)
                        .await
                    {
                        // Honour the per-URI selection count so duplicate
                        // entries are removed one-for-one with the selected
                        // rows rather than all at once.
                        let mut remaining = selection_counts(&uris);
                        for entry in entries {
                            if let Some(ref track_id) = entry.track_id {
                                // Look up the track to get its file path / URI.
                                if let Ok(Some(track)) = crate::db::entities::track::Entity::find_by_id(track_id.clone())
                                    .one(&db)
                                    .await
                                {
                                    let track_uri = url::Url::from_file_path(&track.file_path)
                                        .map(|u| u.to_string())
                                        .unwrap_or_default();
                                    if let Some(count) = remaining.get_mut(track_uri.as_str()) {
                                        if *count > 0 {
                                            *count -= 1;
                                            let _ = mgr.remove_entry(&entry.id).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to open DB for playlist remove");
                }
            }
        });
    });
    action_group.add_action(&remove_action);
    menu.append(
        Some("Remove from Playlist"),
        Some("tracklist-ctx.remove-from-playlist"),
    );
}

/// Build "Add to Playlist" actions (flat list with disabled header).
fn build_add_to_playlist_actions(
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    sidebar_store: &gtk::gio::ListStore,
    sm: &gtk::SortListModel,
    selected: &gtk::Bitset,
    rt_handle: &tokio::runtime::Handle,
) {
    let mut has_playlists = false;

    // Find all regular playlists from the sidebar store.
    let n = sidebar_store.n_items();
    for i in 0..n {
        if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
            if src.backend_type() == "playlist" {
                // Add the "Add to Playlist" header on first playlist found.
                if !has_playlists {
                    has_playlists = true;
                    // Disabled action renders as an unclickable label header.
                    let header_action = gtk::gio::SimpleAction::new("add-to-playlist-header", None);
                    header_action.set_enabled(false);
                    action_group.add_action(&header_action);
                    menu.append(
                        Some("Add to Playlist"),
                        Some("tracklist-ctx.add-to-playlist-header"),
                    );
                }

                let pl_name = src.name();
                let pl_id = src.playlist_id();
                let action_name = format!("add-to-{}", pl_id.replace('-', "_"));

                // Collect selected URIs.
                let selected_uris = collect_selected_uris(sm, selected);

                let rt = rt_handle.clone();
                let add_action = gtk::gio::SimpleAction::new(&action_name, None);
                let uris = selected_uris;
                let pid = pl_id.clone();
                add_action.connect_activate(move |_, _| {
                    let uris = uris.clone();
                    let pid = pid.clone();
                    rt.spawn(async move {
                        match crate::db::connection::init_db().await {
                            Ok(db) => {
                                let mgr = crate::local::playlist_manager::PlaylistManager::new(db.clone());
                                let mut added = 0usize;
                                let mut skipped = 0usize;
                                for uri in &uris {
                                    // Convert file:// URI back to path, find track in DB.
                                    // Remote (http/https) tracks have no local DB row and
                                    // cannot be added to a local playlist — count them as
                                    // skipped rather than dropping them silently.
                                    let mut ok = false;
                                    if let Ok(url) = url::Url::parse(uri) {
                                        if let Ok(path) = url.to_file_path() {
                                            let path_str = path.to_string_lossy().to_string();
                                            if let Ok(Some(track)) = <crate::db::entities::track::Entity as sea_orm::EntityTrait>::find()
                                                .filter(<crate::db::entities::track::Column as sea_orm::ColumnTrait>::eq(
                                                    &crate::db::entities::track::Column::FilePath,
                                                    &path_str,
                                                ))
                                                .one(&db)
                                                .await
                                            {
                                                ok = mgr.add_track(&pid, &track).await.is_ok();
                                            }
                                        }
                                    }
                                    if ok {
                                        added += 1;
                                    } else {
                                        skipped += 1;
                                    }
                                }
                                if skipped > 0 {
                                    tracing::warn!(
                                        playlist = %pid,
                                        added,
                                        skipped,
                                        "Some tracks could not be added (remote or missing tracks aren't supported in local playlists)"
                                    );
                                }
                                // Report the count actually inserted, not the
                                // full selection size.
                                tracing::info!(playlist = %pid, count = added, "Tracks added to playlist");
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to open DB for playlist add");
                            }
                        }
                    });
                });
                action_group.add_action(&add_action);
                menu.append(
                    Some(&format!("  {pl_name}")),
                    Some(&format!("tracklist-ctx.{action_name}")),
                );
            }
        }
    }
}

/// Build the "Properties…" action for selected tracks.
fn build_properties_action(
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    gesture: &gtk::GestureClick,
    sm: &gtk::SortListModel,
    selected: &gtk::Bitset,
) {
    let props_action = gtk::gio::SimpleAction::new("properties", None);
    let sm_for_props = sm.clone();
    let selected_for_props = selected.clone();
    let win_for_props = gesture.widget().and_then(|w| {
        w.root()
            .and_then(|r| r.downcast::<adw::ApplicationWindow>().ok())
    });

    props_action.connect_activate(move |_, _| {
        let Some(ref win) = win_for_props else { return };

        // Collect TrackInfo for selected tracks.
        let mut track_infos = Vec::new();
        let mut pos = 0u32;
        while pos < sm_for_props.n_items() {
            if selected_for_props.contains(pos) {
                if let Some(item) = sm_for_props.item(pos) {
                    if let Some(track) = item.downcast_ref::<TrackObject>() {
                        let uri = track.uri();
                        // Only show properties for local file:// tracks.
                        if uri.starts_with("file://") {
                            track_infos.push(super::properties_dialog::TrackInfo {
                                uri,
                                title: track.title(),
                                artist: track.artist(),
                                album: track.album(),
                                genre: track.genre(),
                                year: track.year_display(),
                                track_number: if track.track_number() > 0 {
                                    track.track_number().to_string()
                                } else {
                                    String::new()
                                },
                                disc_number: if track.disc_number() > 0 {
                                    track.disc_number().to_string()
                                } else {
                                    String::new()
                                },
                                format: track.format(),
                                bitrate: track.bitrate_display(),
                                sample_rate: track.sample_rate_display(),
                                duration: track.duration_display(),
                            });
                        }
                    }
                }
            }
            pos += 1;
        }

        if track_infos.is_empty() {
            return;
        }

        super::properties_dialog::show_properties_dialog(win, &track_infos);
    });

    action_group.add_action(&props_action);
    menu.append(Some("Properties…"), Some("tracklist-ctx.properties"));
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// Build a map of `uri -> number of selected rows with that uri`.
///
/// A playlist may legitimately contain the same track more than once, so
/// removal must honour the count of selected rows (remove exactly N
/// occurrences) rather than treating the selection as a set and deleting
/// every matching entry.
fn selection_counts(uris: &[String]) -> std::collections::HashMap<&str, usize> {
    let mut counts = std::collections::HashMap::new();
    for uri in uris {
        *counts.entry(uri.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Collect URIs of selected tracks from the sort model.
fn collect_selected_uris(sm: &gtk::SortListModel, selected: &gtk::Bitset) -> Vec<String> {
    let mut uris = Vec::new();
    let mut pos = 0u32;
    while pos < sm.n_items() {
        if selected.contains(pos) {
            if let Some(item) = sm.item(pos) {
                if let Some(track) = item.downcast_ref::<TrackObject>() {
                    uris.push(track.uri());
                }
            }
        }
        pos += 1;
    }
    uris
}

/// Traverse a `PopoverMenu`'s widget tree and disable scrollbars on any
/// internal `ScrolledWindow`.  GTK4's `PopoverMenu::from_model()` wraps
/// its content in a `ScrolledWindow` that adds unnecessary scrollbars
/// for small menus (e.g., "Add to Playlist" with only 2–3 entries).
fn disable_popover_scrollbars(popover: &gtk::PopoverMenu) {
    fn walk(widget: &gtk::Widget) {
        if let Some(sw) = widget.downcast_ref::<gtk::ScrolledWindow>() {
            sw.set_hscrollbar_policy(gtk::PolicyType::Never);
            sw.set_vscrollbar_policy(gtk::PolicyType::Never);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            walk(&c);
            child = c.next_sibling();
        }
    }
    walk(popover.upcast_ref::<gtk::Widget>());
}
