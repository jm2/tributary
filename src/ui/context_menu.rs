//! Pointer and keyboard context menu on the tracklist `ColumnView`.
//!
//! Handles "Remove from Playlist", "Add to Playlist", and "Properties…"
//! actions triggered from right-clicking selected tracks or pressing the
//! platform context-menu key / Shift+F10.

use adw::prelude::*;
use sea_orm::{EntityTrait, QueryFilter};
use std::rc::Rc;

use super::browser;
use super::objects::{SourceObject, TrackObject};
use super::tracklist;
use super::window_state::WindowState;

const CONTEXT_MENU_ACTION_GROUP: &str = "tracklist-ctx";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuControllerPlan {
    EventControllerKeyBubble,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuActionOwner {
    Popover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextMenuInteractionPlan {
    keyboard_controller: ContextMenuControllerPlan,
    has_popup: bool,
    accessible_key_shortcuts: &'static str,
    action_owner: ContextMenuActionOwner,
}

const CONTEXT_MENU_INTERACTION: ContextMenuInteractionPlan = ContextMenuInteractionPlan {
    keyboard_controller: ContextMenuControllerPlan::EventControllerKeyBubble,
    has_popup: true,
    // GTK/GDK calls the physical key `Menu`; the GTK accessible shortcut
    // grammar uses the standardized `ContextMenu` token.
    accessible_key_shortcuts: "Shift+F10 ContextMenu",
    action_owner: ContextMenuActionOwner::Popover,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionSnapshot {
    positions: Vec<u32>,
}

impl SelectionSnapshot {
    fn from_positions(positions: impl IntoIterator<Item = u32>) -> Option<Self> {
        let positions = positions.into_iter().collect::<Vec<_>>();
        (!positions.is_empty()).then_some(Self { positions })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextMenuPopupPlan {
    selection: SelectionSnapshot,
    action_owner: ContextMenuActionOwner,
}

impl ContextMenuPopupPlan {
    fn from_positions(positions: impl IntoIterator<Item = u32>) -> Option<Self> {
        Some(Self {
            selection: SelectionSnapshot::from_positions(positions)?,
            action_owner: CONTEXT_MENU_INTERACTION.action_owner,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardContextMenuPropagation {
    Proceed,
    Stop,
}

impl KeyboardContextMenuPropagation {
    fn into_gtk(self) -> gtk::glib::Propagation {
        match self {
            Self::Proceed => gtk::glib::Propagation::Proceed,
            Self::Stop => gtk::glib::Propagation::Stop,
        }
    }
}

fn keyboard_context_menu_propagation(
    is_trigger: bool,
    popup_opened: bool,
) -> KeyboardContextMenuPropagation {
    if is_trigger && popup_opened {
        KeyboardContextMenuPropagation::Stop
    } else {
        KeyboardContextMenuPropagation::Proceed
    }
}

fn is_keyboard_context_menu_trigger(key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType) -> bool {
    use gtk::gdk::ModifierType;

    // Lock/legacy modifier state (for example NumLock's X11 Mod2 bit) is
    // ambient, not a chord. Keep only modifiers that participate in shortcuts,
    // then accept the exact standard bindings: unmodified Menu or Shift+F10.
    // In particular, Shift+Menu remains available to an ancestor binding.
    let effective = modifiers
        & (ModifierType::SHIFT_MASK
            | ModifierType::CONTROL_MASK
            | ModifierType::ALT_MASK
            | ModifierType::SUPER_MASK);
    (key == gtk::gdk::Key::Menu && effective.is_empty())
        || (key == gtk::gdk::Key::F10 && effective == ModifierType::SHIFT_MASK)
}

fn expose_context_menu_accessibility(column_view: &gtk::ColumnView) {
    column_view.update_property(&[
        gtk::accessible::Property::HasPopup(CONTEXT_MENU_INTERACTION.has_popup),
        gtk::accessible::Property::KeyShortcuts(CONTEXT_MENU_INTERACTION.accessible_key_shortcuts),
    ]);
}

/// Wire pointer and keyboard context-menu access on the tracklist.
///
/// Right-click retains its exact pointer anchor. The Menu key and Shift+F10
/// open the same selection-snapshotted action model relative to the focused
/// tracklist, and are consumed only when a non-empty menu was opened.
pub fn setup_context_menu(state: &WindowState) {
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

    let popup_menu = Rc::new(
        move |cv: &gtk::ColumnView, anchor: Option<gtk::gdk::Rectangle>| {
            let active_key = active_source_key.borrow().clone();
            let is_playlist_view = active_key.starts_with("playlist:");

            // Collect selected track URIs from the MultiSelection model.
            let selection_model = cv.model();
            let Some(sel) = selection_model.and_then(|m| m.downcast::<gtk::MultiSelection>().ok())
            else {
                return false;
            };

            let selected = sel.selection();
            let Some(popup_plan) = ContextMenuPopupPlan::from_positions(
                (0..sm.n_items()).filter(|position| selected.contains(*position)),
            ) else {
                return false;
            };

            let menu = gtk::gio::Menu::new();
            let action_group = gtk::gio::SimpleActionGroup::new();

            if is_playlist_view {
                build_remove_from_playlist_action(
                    &menu,
                    &action_group,
                    &active_key,
                    &sm,
                    &popup_plan.selection,
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
                    &popup_plan.selection,
                    &rt_handle,
                );
            }

            // ── Properties… ──────────────────────────────────────────
            let automatic_device = active_source_is_automatic_device(&sidebar_store, &active_key);
            build_properties_action(
                &menu,
                &action_group,
                cv,
                &sm,
                &popup_plan.selection,
                automatic_device,
            );

            if menu.n_items() == 0 {
                return false;
            }

            let popover = gtk::PopoverMenu::from_model(Some(&menu));
            popover.set_parent(cv);
            match popup_plan.action_owner {
                ContextMenuActionOwner::Popover => {
                    popover.insert_action_group(CONTEXT_MENU_ACTION_GROUP, Some(&action_group));
                }
            }
            if let Some(anchor) = anchor {
                popover.set_pointing_to(Some(&anchor));
            }

            // Disable the internal ScrolledWindow that GTK4 PopoverMenu
            // creates — it adds unnecessary scrollbars for small menus.
            disable_popover_scrollbars(&popover);

            // Popovers created for a snapshot are one-shot. Explicitly detach the
            // closed widget so repeated keyboard or pointer invocations do not
            // retain obsolete action groups or captured selections.
            popover.connect_closed(|popover| popover.unparent());
            popover.popup();
            true
        },
    );

    let gesture = gtk::GestureClick::new();
    gesture.set_button(3); // right-click
    {
        let popup_menu = Rc::clone(&popup_menu);
        gesture.connect_pressed(move |gesture, _n_press, x, y| {
            let Some(cv) = gesture
                .widget()
                .and_then(|widget| widget.downcast::<gtk::ColumnView>().ok())
            else {
                return;
            };
            let anchor = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            popup_menu(&cv, Some(anchor));
        });
    }
    state.column_view.add_controller(gesture);

    let key_controller = match CONTEXT_MENU_INTERACTION.keyboard_controller {
        ContextMenuControllerPlan::EventControllerKeyBubble => {
            let controller = gtk::EventControllerKey::new();
            controller.set_propagation_phase(gtk::PropagationPhase::Bubble);
            controller
        }
    };
    key_controller.connect_key_pressed(move |controller, key, _keycode, modifiers| {
        let is_trigger = is_keyboard_context_menu_trigger(key, modifiers);
        if !is_trigger {
            return keyboard_context_menu_propagation(false, false).into_gtk();
        }
        let Some(cv) = controller
            .widget()
            .and_then(|widget| widget.downcast::<gtk::ColumnView>().ok())
        else {
            return keyboard_context_menu_propagation(true, false).into_gtk();
        };

        keyboard_context_menu_propagation(true, popup_menu(&cv, None)).into_gtk()
    });
    state.column_view.add_controller(key_controller);
    expose_context_menu_accessibility(&state.column_view);
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
    selection: &SelectionSnapshot,
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
    let selected_uris = collect_selected_uris(sm, selection);

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
    selection: &SelectionSnapshot,
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
                let selected_uris = collect_selected_uris(sm, selection);

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
    column_view: &gtk::ColumnView,
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
    automatic_device: bool,
) {
    // Snapshot the exact selection while building the menu. Properties is an
    // all-or-none local-file operation: silently dropping a malformed or
    // remote row would let a batch edit only an unexpected subset.
    let mut track_infos = Vec::new();
    for &position in &selection.positions {
        let Some(item) = sm.item(position) else {
            return;
        };
        let Some(track) = item.downcast_ref::<TrackObject>() else {
            return;
        };
        let Some(path) = local_file_path(&track.uri()) else {
            return;
        };
        track_infos.push(super::properties_dialog::TrackInfo {
            path,
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            genre: track.genre(),
            composer: track.composer(),
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
    if track_infos.is_empty() {
        return;
    }

    let props_action = gtk::gio::SimpleAction::new("properties", None);
    let win_for_props = column_view
        .root()
        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok());

    props_action.connect_activate(move |_, _| {
        let Some(ref win) = win_for_props else { return };
        super::properties_dialog::show_properties_dialog(win, &track_infos, automatic_device);
    });

    action_group.add_action(&props_action);
    menu.append(Some("Properties…"), Some("tracklist-ctx.properties"));
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

fn local_file_path(uri: &str) -> Option<std::path::PathBuf> {
    let url = url::Url::parse(uri).ok()?;
    (url.scheme() == "file")
        .then(|| url.to_file_path().ok())
        .flatten()
}

/// Match the active logical source against exact sidebar metadata. Opaque
/// source keys and mount-path spellings are not type information.
fn active_source_is_automatic_device(
    sidebar_store: &gtk::gio::ListStore,
    active_source_key: &str,
) -> bool {
    (0..sidebar_store.n_items()).any(|position| {
        sidebar_store
            .item(position)
            .and_downcast::<SourceObject>()
            .is_some_and(|source| {
                source.backend_type() == "usb-device" && source.source_key() == active_source_key
            })
    })
}

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
fn collect_selected_uris(sm: &gtk::SortListModel, selection: &SelectionSnapshot) -> Vec<String> {
    let mut uris = Vec::new();
    for &position in &selection.positions {
        if let Some(item) = sm.item(position) {
            if let Some(track) = item.downcast_ref::<TrackObject>() {
                uris.push(track.uri());
            }
        }
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn properties_path_conversion_is_local_and_fail_closed() {
        let path = std::env::temp_dir().join("tributary properties fixture.flac");
        let uri = url::Url::from_file_path(&path)
            .expect("absolute fixture path")
            .to_string();

        assert_eq!(local_file_path(&uri), Some(path));
        assert_eq!(local_file_path("https://example.test/song.flac"), None);
        assert_eq!(local_file_path("file://%"), None);
        assert_eq!(local_file_path("not a URI"), None);
    }

    #[test]
    fn automatic_device_context_uses_exact_source_metadata() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        store.append(&SourceObject::source(
            "Local",
            "local",
            "folder-music-symbolic",
        ));
        store.append(&SourceObject::removable_device(
            "Player",
            "device:opaque-id",
            PathBuf::from("/media/player"),
        ));

        assert!(active_source_is_automatic_device(
            &store,
            "device:opaque-id"
        ));
        assert!(!active_source_is_automatic_device(&store, "/media/player"));
        assert!(!active_source_is_automatic_device(&store, "device:opaque"));
        assert!(!active_source_is_automatic_device(&store, "local"));
    }

    #[test]
    fn keyboard_context_menu_plan_pins_wiring_snapshot_and_propagation() {
        use gtk::gdk::{Key, ModifierType};

        assert!(is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::empty()
        ));
        assert!(is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::LOCK_MASK
        ));
        assert!(is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK
        ));
        assert!(is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK | ModifierType::LOCK_MASK
        ));
        // GDK4 no longer names the legacy X11 Mod2 mask, but key state can
        // still carry its raw bit while NumLock is active.
        let ambient_mod2 = ModifierType::from_bits_retain(1 << 4);
        assert!(is_keyboard_context_menu_trigger(Key::Menu, ambient_mod2));
        assert!(is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK | ambient_mod2
        ));

        assert!(!is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::empty()
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK | ModifierType::CONTROL_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::SHIFT_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::SHIFT_MASK | ModifierType::LOCK_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::ALT_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::F9,
            ModifierType::SHIFT_MASK
        ));

        assert_eq!(
            CONTEXT_MENU_INTERACTION,
            ContextMenuInteractionPlan {
                keyboard_controller: ContextMenuControllerPlan::EventControllerKeyBubble,
                has_popup: true,
                accessible_key_shortcuts: "Shift+F10 ContextMenu",
                action_owner: ContextMenuActionOwner::Popover,
            }
        );

        assert_eq!(
            keyboard_context_menu_propagation(false, false),
            KeyboardContextMenuPropagation::Proceed
        );
        assert_eq!(
            keyboard_context_menu_propagation(true, false),
            KeyboardContextMenuPropagation::Proceed
        );
        assert_eq!(
            keyboard_context_menu_propagation(true, true),
            KeyboardContextMenuPropagation::Stop
        );

        assert!(ContextMenuPopupPlan::from_positions([]).is_none());
        let mut live_selection = vec![1, 3];
        let popup_plan = ContextMenuPopupPlan::from_positions(live_selection.iter().copied())
            .expect("non-empty selection must produce a popup plan");
        live_selection.clear();
        live_selection.push(4);
        assert_eq!(popup_plan.selection.positions, vec![1, 3]);
        assert_eq!(popup_plan.action_owner, ContextMenuActionOwner::Popover);
    }
}
