//! Sidebar — `GtkListView` of media backend sources, grouped by category.
//!
//! The sidebar is driven by a `gio::ListStore<SourceObject>` that can be
//! mutated at runtime (e.g., to add mDNS-discovered servers).
//!
//! DAAP sources get a monochrome eject button for disconnecting.
//! Manually-added servers get a trash button for removal.

use gtk::glib::variant::{FromVariant, ToVariant};
use gtk::prelude::*;
use gtk::{gio, glib};

use super::objects::{PlaylistSidebarKind, SourceObject};
use tracing::debug;

const PLAYLIST_POPUP_ACTION_GROUP: &str = "playlist";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistPopupActionOwner {
    Popover,
}

const PLAYLIST_POPUP_ACTION_OWNER: PlaylistPopupActionOwner = PlaylistPopupActionOwner::Popover;

/// Playlist action emitted from the sidebar context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistAction {
    /// Create a new regular playlist.
    CreateRegular,
    /// Create a new smart playlist.
    CreateSmart,
    /// Rename a playlist (id).
    Rename(String),
    /// Delete a playlist (id).
    Delete(String),
    /// Edit smart playlist rules (id).
    EditSmart(String),
    /// Import a playlist from an XSPF file.
    ImportPlaylist,
    /// Browse playlists exposed by connected servers.
    BrowseServerPlaylists,
    /// Export a playlist to an XSPF file (id).
    ExportPlaylist(String),
}

/// Action represented by the recycled row's trailing button.
#[derive(Debug, PartialEq, Eq)]
enum SidebarButtonAction {
    OpenPlaylistMenu,
    Disconnect(String),
    Delete(String),
}

/// Resolve the trailing-button action from the source bound right now.
///
/// This intentionally derives no state from an earlier `bind` invocation:
/// `GtkListItem` widgets are recycled, and several connection flows force a
/// remove/reinsert to refresh a row.
fn sidebar_button_action(source: &SourceObject) -> Option<SidebarButtonAction> {
    if source.is_header() {
        return source
            .is_playlist_header()
            .then_some(SidebarButtonAction::OpenPlaylistMenu);
    }

    if source.connecting() {
        return None;
    }

    if source.backend_type() == "daap" && source.connected() {
        return source
            .source_id()
            .map(|id| SidebarButtonAction::Disconnect(id.to_string()));
    }

    if source.manually_added() && (source.connected() || !source.server_url().is_empty()) {
        return source
            .source_id()
            .map(|id| SidebarButtonAction::Delete(id.to_string()));
    }

    None
}

fn configure_action_button(button: &gtk::Button, action: Option<&SidebarButtonAction>) {
    match action {
        Some(SidebarButtonAction::OpenPlaylistMenu) => {
            button.set_icon_name("list-add-symbolic");
            button.set_tooltip_text(Some(rust_i18n::t!("sidebar.new_playlist_menu").as_ref()));
            button.set_visible(true);
        }
        Some(SidebarButtonAction::Disconnect(_)) => {
            button.set_icon_name("media-eject-symbolic");
            button.set_tooltip_text(Some("Disconnect"));
            button.set_visible(true);
        }
        Some(SidebarButtonAction::Delete(_)) => {
            button.set_icon_name("user-trash-symbolic");
            button.set_tooltip_text(Some("Remove server"));
            button.set_visible(true);
        }
        None => {
            button.set_icon_name("");
            button.set_tooltip_text(None);
            button.set_visible(false);
        }
    }
}

/// Emit a non-menu row action. Returns `true` when the playlist menu should
/// be opened instead.
fn emit_sidebar_button_action(
    action: SidebarButtonAction,
    disconnect_tx: &async_channel::Sender<String>,
    delete_tx: &async_channel::Sender<String>,
) -> bool {
    match action {
        SidebarButtonAction::OpenPlaylistMenu => true,
        SidebarButtonAction::Disconnect(source_key) => {
            let _ = disconnect_tx.try_send(source_key);
            false
        }
        SidebarButtonAction::Delete(source_key) => {
            let _ = delete_tx.try_send(source_key);
            false
        }
    }
}

/// Encode one recycled row's current action as the immutable activation
/// target installed by the factory's `bind` callback.
///
/// `GtkListItem` and its child widgets outlive any individual model binding.
/// Keeping the current source in the button's action target means the one
/// setup-time signal handler never captures a source from an earlier bind,
/// while `unbind` can explicitly revoke the target before GTK recycles it.
fn sidebar_button_action_target(action: Option<&SidebarButtonAction>) -> glib::Variant {
    match action {
        Some(SidebarButtonAction::OpenPlaylistMenu) => ("open", "").to_variant(),
        Some(SidebarButtonAction::Disconnect(source_key)) => {
            ("disconnect", source_key.as_str()).to_variant()
        }
        Some(SidebarButtonAction::Delete(source_key)) => {
            ("delete", source_key.as_str()).to_variant()
        }
        None => ("none", "").to_variant(),
    }
}

fn sidebar_button_action_from_target(target: &glib::Variant) -> Option<SidebarButtonAction> {
    let (kind, source_key) = <(String, String)>::from_variant(target)?;
    match kind.as_str() {
        "open" if source_key.is_empty() => Some(SidebarButtonAction::OpenPlaylistMenu),
        "disconnect" if !source_key.is_empty() => Some(SidebarButtonAction::Disconnect(source_key)),
        "delete" if !source_key.is_empty() => Some(SidebarButtonAction::Delete(source_key)),
        "none" if source_key.is_empty() => None,
        _ => None,
    }
}

/// Build the single row-lifetime action installed during factory setup.
/// Rebinding changes only the action target; it never adds another handler.
fn sidebar_row_action(
    disconnect_tx: &async_channel::Sender<String>,
    delete_tx: &async_channel::Sender<String>,
    open_playlist_menu: impl Fn() + 'static,
) -> gio::SimpleAction {
    let parameter_type = glib::VariantTy::new("(ss)").expect("valid sidebar action type");
    let action = gio::SimpleAction::new("invoke", Some(parameter_type));
    let disconnect_tx = disconnect_tx.clone();
    let delete_tx = delete_tx.clone();
    action.connect_activate(move |_, target| {
        let Some(action) = target.and_then(sidebar_button_action_from_target) else {
            return;
        };
        if emit_sidebar_button_action(action, &disconnect_tx, &delete_tx) {
            open_playlist_menu();
        }
    });
    action
}

fn playlist_creation_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(
        Some(rust_i18n::t!("sidebar.new_playlist_menu").as_ref()),
        Some("pl-add.create-regular"),
    );
    menu.append(
        Some(rust_i18n::t!("sidebar.new_smart_playlist_menu").as_ref()),
        Some("pl-add.create-smart"),
    );
    menu.append(
        Some(rust_i18n::t!("playlist_io.import_menu").as_ref()),
        Some("pl-add.import"),
    );
    menu.append(
        Some(rust_i18n::t!("server_playlists.browse_menu").as_ref()),
        Some("pl-add.browse-server-playlists"),
    );
    menu
}

fn playlist_creation_action_group(
    tx: &async_channel::Sender<PlaylistAction>,
) -> gio::SimpleActionGroup {
    let action_group = gio::SimpleActionGroup::new();

    let tx_regular = tx.clone();
    let regular = gio::SimpleAction::new("create-regular", None);
    regular.connect_activate(move |_, _| {
        let _ = tx_regular.try_send(PlaylistAction::CreateRegular);
    });
    action_group.add_action(&regular);

    let tx_smart = tx.clone();
    let smart = gio::SimpleAction::new("create-smart", None);
    smart.connect_activate(move |_, _| {
        let _ = tx_smart.try_send(PlaylistAction::CreateSmart);
    });
    action_group.add_action(&smart);

    let tx_import = tx.clone();
    let import = gio::SimpleAction::new("import", None);
    import.connect_activate(move |_, _| {
        let _ = tx_import.try_send(PlaylistAction::ImportPlaylist);
    });
    action_group.add_action(&import);

    let tx_browse = tx.clone();
    let browse = gio::SimpleAction::new("browse-server-playlists", None);
    browse.connect_activate(move |_, _| {
        let _ = tx_browse.try_send(PlaylistAction::BrowseServerPlaylists);
    });
    action_group.add_action(&browse);

    action_group
}

/// Build one immutable action snapshot for a playlist context-menu popover.
///
/// A `GtkListItem` and its row widget may be rebound while the menu remains
/// open, so the actions must capture the target ID once and be owned by the
/// one-shot popover rather than by the recycled row.
fn playlist_popup_action_group(
    tx: &async_channel::Sender<PlaylistAction>,
    playlist_id: Option<&str>,
) -> gio::SimpleActionGroup {
    let action_group = playlist_creation_action_group(tx);
    let Some(playlist_id) = playlist_id else {
        return action_group;
    };
    let playlist_id = playlist_id.to_string();

    let tx_rename = tx.clone();
    let pid = playlist_id.clone();
    let rename = gio::SimpleAction::new("rename", None);
    rename.connect_activate(move |_, _| {
        let _ = tx_rename.try_send(PlaylistAction::Rename(pid.clone()));
    });
    action_group.add_action(&rename);

    let tx_delete = tx.clone();
    let pid = playlist_id.clone();
    let delete = gio::SimpleAction::new("delete", None);
    delete.connect_activate(move |_, _| {
        let _ = tx_delete.try_send(PlaylistAction::Delete(pid.clone()));
    });
    action_group.add_action(&delete);

    let tx_edit = tx.clone();
    let pid = playlist_id.clone();
    let edit_smart = gio::SimpleAction::new("edit-smart", None);
    edit_smart.connect_activate(move |_, _| {
        let _ = tx_edit.try_send(PlaylistAction::EditSmart(pid.clone()));
    });
    action_group.add_action(&edit_smart);

    let tx_export = tx.clone();
    let export = gio::SimpleAction::new("export", None);
    export.connect_activate(move |_, _| {
        let _ = tx_export.try_send(PlaylistAction::ExportPlaylist(playlist_id.clone()));
    });
    action_group.add_action(&export);

    action_group
}

/// Build the source sidebar.
///
/// Returns `(sidebar_box, ListStore, SingleSelection, disconnect_rx, delete_rx, add_button, playlist_action_rx)`.
///
/// * `disconnect_rx` emits the stable `SourceId` of a DAAP source when the
///   user clicks its eject button.
/// * `delete_rx` emits the stable `SourceId` of a manually-added source when
///   the user clicks its trash button.
/// * `add_button` is the `+` button for adding manual servers (wired in `window.rs`).
/// * `playlist_action_rx` emits playlist CRUD actions from the context menu.
pub fn build_sidebar(
    initial_sources: &[SourceObject],
) -> (
    gtk::Box,
    gio::ListStore,
    gtk::SingleSelection,
    async_channel::Receiver<String>,
    async_channel::Receiver<String>,
    gtk::Button,
    async_channel::Receiver<PlaylistAction>,
) {
    let store = gio::ListStore::new::<SourceObject>();
    for src in initial_sources {
        store.append(src);
    }

    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    // Skip the "Local" header row — select the first actual source.
    selection.set_selected(1);

    // Channel for DAAP disconnect (eject) requests.
    let (disconnect_tx, disconnect_rx) = async_channel::unbounded::<String>();
    // Channel for manual server delete (trash) requests.
    let (delete_tx, delete_rx) = async_channel::unbounded::<String>();

    // ── Playlist action channel (shared by header "+" button and context menu) ──
    let (playlist_action_tx, playlist_action_rx) = async_channel::unbounded::<PlaylistAction>();

    let factory = gtk::SignalListItemFactory::new();

    {
        let store_for_setup = store.clone();
        let tx_for_setup = playlist_action_tx.clone();
        let disconnect_tx_for_setup = disconnect_tx.clone();
        let delete_tx_for_setup = delete_tx.clone();
        factory.connect_setup(move |_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("ListItem expected");
            let row_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_start(8)
                .margin_end(8)
                .margin_top(4)
                .margin_bottom(4)
                .build();

            let icon = gtk::Image::builder().pixel_size(16).build();
            let spinner = gtk::Spinner::builder()
                .spinning(true)
                .width_request(16)
                .height_request(16)
                .visible(false)
                .build();
            let label = gtk::Label::builder()
                .halign(gtk::Align::Start)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            // Action button: playlist add, DAAP eject, or manual trash.
            let action_btn = gtk::Button::builder()
                .css_classes(["flat", "circular"])
                .visible(false)
                .build();

            row_box.append(&icon);
            row_box.append(&spinner);
            row_box.append(&label);
            row_box.append(&action_btn);
            list_item.set_child(Some(&row_box));

            // Connect the recycled button exactly once. Every bind replaces
            // its immutable action target and unbind revokes it, so a prior
            // source never remains reachable through this row-lifetime
            // handler.
            let playlist_menu = playlist_creation_menu();
            let playlist_actions = playlist_creation_action_group(&tx_for_setup);
            action_btn.insert_action_group("pl-add", Some(&playlist_actions));

            let button_for_menu = action_btn.downgrade();
            let row_action =
                sidebar_row_action(&disconnect_tx_for_setup, &delete_tx_for_setup, move || {
                    let Some(button) = button_for_menu.upgrade() else {
                        return;
                    };
                    let popover = gtk::PopoverMenu::from_model(Some(&playlist_menu));
                    popover.set_parent(&button);
                    popover.connect_closed(|popover| popover.unparent());
                    popover.popup();
                });
            let row_actions = gio::SimpleActionGroup::new();
            row_actions.add_action(&row_action);
            row_box.insert_action_group("sidebar-row", Some(&row_actions));
            action_btn.set_action_name(Some("sidebar-row.invoke"));
            action_btn.set_action_target_value(Some(&sidebar_button_action_target(None)));

            // Per-row right-click gesture.
            //
            // The gesture is attached to row_box (not the ListView) because
            // header rows are non-selectable, which means a ListView-level
            // handler that resolves the target via `selection.selected()`
            // can never see the "Playlists" header.  Per-row gestures sidestep
            // that entirely: they resolve the target via `list_item.position()`,
            // which tracks the current binding even for non-selectable rows.
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            let store_for_gesture = store_for_setup.clone();
            let tx_for_gesture = tx_for_setup.clone();
            let list_item_for_gesture = list_item.clone();
            let row_box_for_gesture = row_box.clone();
            gesture.connect_pressed(move |_, _n_press, x, y| {
                let pos = list_item_for_gesture.position();
                let Some(item) = store_for_gesture.item(pos) else {
                    return;
                };
                let Some(src) = item.downcast_ref::<SourceObject>() else {
                    return;
                };

                let playlist_kind = src.playlist_kind();
                let is_playlist = src.is_playlist();
                let is_playlist_header = src.is_playlist_header();
                if !is_playlist && !is_playlist_header {
                    return;
                }

                let menu = gtk::gio::Menu::new();
                if is_playlist_header {
                    menu.append(
                        Some(rust_i18n::t!("sidebar.new_playlist_menu").as_ref()),
                        Some("playlist.create-regular"),
                    );
                    menu.append(
                        Some(rust_i18n::t!("sidebar.new_smart_playlist_menu").as_ref()),
                        Some("playlist.create-smart"),
                    );
                    menu.append(
                        Some(rust_i18n::t!("playlist_io.import_menu").as_ref()),
                        Some("playlist.import"),
                    );
                } else if matches!(
                    playlist_kind,
                    Some(PlaylistSidebarKind::EditableRegular | PlaylistSidebarKind::EditableSmart)
                ) {
                    menu.append(
                        Some(rust_i18n::t!("sidebar.rename").as_ref()),
                        Some("playlist.rename"),
                    );
                    menu.append(
                        Some(rust_i18n::t!("playlist_io.export_menu").as_ref()),
                        Some("playlist.export"),
                    );
                    menu.append(
                        Some(rust_i18n::t!("sidebar.delete").as_ref()),
                        Some("playlist.delete"),
                    );
                    if playlist_kind == Some(PlaylistSidebarKind::EditableSmart) {
                        menu.append(
                            Some(rust_i18n::t!("sidebar.edit_smart_playlist").as_ref()),
                            Some("playlist.edit-smart"),
                        );
                    }
                } else {
                    // Pull mirrors deliberately have no ordinary rename,
                    // export, delete, or smart-rule action. Record E adds
                    // their dedicated synchronization/recovery actions.
                    return;
                }

                let pid = src.playlist_id();
                let action_group = playlist_popup_action_group(
                    &tx_for_gesture,
                    is_playlist.then_some(pid.as_str()),
                );

                let popover = gtk::PopoverMenu::from_model(Some(&menu));
                popover.set_parent(&row_box_for_gesture);
                match PLAYLIST_POPUP_ACTION_OWNER {
                    PlaylistPopupActionOwner::Popover => popover
                        .insert_action_group(PLAYLIST_POPUP_ACTION_GROUP, Some(&action_group)),
                }
                #[allow(clippy::cast_possible_truncation)]
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.connect_closed(|popover| popover.unparent());
                popover.popup();
            });
            row_box.add_controller(gesture);
        });
    }

    {
        factory.connect_bind(move |_, list_item| {
            let list_item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("ListItem expected");
            let obj = list_item
                .item()
                .and_downcast::<SourceObject>()
                .expect("SourceObject expected");
            let row_box = list_item
                .child()
                .and_downcast::<gtk::Box>()
                .expect("Box expected");

            let icon = row_box
                .first_child()
                .and_downcast::<gtk::Image>()
                .expect("Image expected");
            let spinner = icon
                .next_sibling()
                .and_downcast::<gtk::Spinner>()
                .expect("Spinner expected");
            let label = spinner
                .next_sibling()
                .and_downcast::<gtk::Label>()
                .expect("Label expected");
            let action_btn = label
                .next_sibling()
                .and_downcast::<gtk::Button>()
                .expect("Button expected");

            // Every property which varies by binding is initialized before
            // applying the new object. GtkListItem and its child widgets are
            // recycled, so hidden header/playlist/connection state must not
            // survive from the prior row.
            icon.set_icon_name(None::<&str>);
            icon.set_visible(false);
            icon.set_tooltip_text(None);
            icon.reset_property(gtk::AccessibleProperty::Label);
            spinner.set_visible(false);
            label.set_text("");
            label.reset_property(gtk::AccessibleProperty::Description);
            label.remove_css_class("heading");
            label.remove_css_class("dim-label");
            label.set_margin_top(0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            list_item.set_activatable(false);
            list_item.set_selectable(false);
            row_box.set_tooltip_text(None);
            action_btn.set_action_target_value(Some(&sidebar_button_action_target(None)));
            configure_action_button(&action_btn, None);

            if obj.is_header() {
                icon.set_visible(false);
                spinner.set_visible(false);
                label.set_text(&obj.name());
                label.add_css_class("heading");
                label.add_css_class("dim-label");
                label.set_margin_top(8);
                label.set_ellipsize(gtk::pango::EllipsizeMode::None);
                list_item.set_activatable(false);
                list_item.set_selectable(false);
            } else {
                label.remove_css_class("heading");
                label.set_margin_top(0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_text(&obj.name());
                list_item.set_activatable(true);
                list_item.set_selectable(true);

                if obj.connecting() {
                    // Auth in progress — show spinner instead of icon.
                    icon.set_visible(false);
                    spinner.set_visible(true);
                    label.add_css_class("dim-label");
                } else if !obj.connected() && !obj.server_url().is_empty() {
                    // Discovered/manual but not yet authenticated.
                    icon.set_visible(true);
                    if obj.requires_password() {
                        // Password-protected — show lock icon.
                        icon.set_icon_name(Some("system-lock-screen-symbolic"));
                        label.add_css_class("dim-label");
                    } else {
                        // Open / passwordless — show normal server icon.
                        icon.set_icon_name(Some("network-server-symbolic"));
                        label.remove_css_class("dim-label");
                    }
                    spinner.set_visible(false);
                } else {
                    // Connected or local source — normal icon.
                    icon.set_visible(true);
                    icon.set_icon_name(Some(&obj.icon_name()));
                    spinner.set_visible(false);
                    label.remove_css_class("dim-label");
                }

                if let Some(status_key) = obj.linked_playlist_status_key() {
                    let status = rust_i18n::t!(status_key).into_owned();
                    // The whole row exposes hover help, while the visible
                    // playlist name carries the state as its accessible
                    // description. Both are cleared before every bind and
                    // again on unbind so recycled rows cannot leak state.
                    row_box.set_tooltip_text(Some(&status));
                    label.update_property(&[gtk::accessible::Property::Description(&status)]);
                }
            }

            let action = sidebar_button_action(&obj);
            configure_action_button(&action_btn, action.as_ref());
            action_btn
                .set_action_target_value(Some(&sidebar_button_action_target(action.as_ref())));
        });
    }

    // Reset presentation on unbind. The click handler itself is row-lifetime
    // state connected once during setup and needs no bind-time cleanup.
    factory.connect_unbind(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");
        if let Some(row_box) = list_item.child().and_downcast::<gtk::Box>() {
            if let Some(icon) = row_box.first_child().and_downcast::<gtk::Image>() {
                icon.set_icon_name(None::<&str>);
                icon.set_visible(false);
                icon.set_tooltip_text(None);
                icon.reset_property(gtk::AccessibleProperty::Label);
                if let Some(spinner) = icon.next_sibling().and_downcast::<gtk::Spinner>() {
                    spinner.set_visible(false);
                    if let Some(label) = spinner.next_sibling().and_downcast::<gtk::Label>() {
                        label.set_text("");
                        label.reset_property(gtk::AccessibleProperty::Description);
                        label.remove_css_class("heading");
                        label.remove_css_class("dim-label");
                        label.set_margin_top(0);
                        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                        // Resolve the setup-owned action button from the
                        // fixed icon -> spinner -> label -> button chain.
                        // A transient popover is also parented to the row and
                        // can therefore be its last child while open; using
                        // `last_child()` would leave the old target live when
                        // that row is unbound.
                        if let Some(btn) = label.next_sibling().and_downcast::<gtk::Button>() {
                            btn.set_action_target_value(Some(&sidebar_button_action_target(None)));
                            configure_action_button(&btn, None);
                        }
                    }
                }
            }
            row_box.set_tooltip_text(None);
        }
        list_item.set_activatable(false);
        list_item.set_selectable(false);
    });

    // Log selection changes
    let selection_clone = selection.clone();
    selection.connect_selection_changed(move |_, _, _| {
        if let Some(item) = selection_clone.selected_item() {
            if let Some(src) = item.downcast_ref::<SourceObject>() {
                if !src.is_header() {
                    debug!(
                        backend = %src.backend_type(),
                        "Sidebar: source selected"
                    );
                }
            }
        }
    });

    let list_view = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .css_classes(["navigation-sidebar"])
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&list_view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .width_request(180)
        .vexpand(true)
        .build();

    // ── Toolbar with + button above the list ────────────────────────
    let add_button = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .tooltip_text(rust_i18n::t!("sidebar.add_server").as_ref())
        .build();

    let toolbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["toolbar"])
        .build();
    toolbar.append(&add_button);

    // Wrap scrolled + toolbar in a vertical box.
    let sidebar_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(180)
        .build();
    sidebar_box.append(&scrolled);
    sidebar_box.append(&toolbar);

    (
        sidebar_box,
        store,
        selection,
        disconnect_rx,
        delete_rx,
        add_button,
        playlist_action_rx,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::ui::objects::HeaderKind;

    fn assert_empty<T>(receiver: &async_channel::Receiver<T>) {
        assert!(
            receiver.try_recv().is_err(),
            "one click must not enqueue an additional action"
        );
    }

    #[test]
    fn recycled_setup_action_dispatches_once_for_only_the_current_binding() {
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (delete_tx, delete_rx) = async_channel::unbounded();
        let opened = Rc::new(Cell::new(0));
        let opened_for_action = opened.clone();
        // This is the exact setup-time GAction used by every production row.
        // The harness drives the same target values installed by bind/unbind,
        // without constructing display-bound GTK widgets on headless CI.
        let action = sidebar_row_action(&disconnect_tx, &delete_tx, move || {
            opened_for_action.set(opened_for_action.get() + 1);
        });
        let activate = |source: Option<&SourceObject>| {
            let current = source.and_then(sidebar_button_action);
            let target = sidebar_button_action_target(current.as_ref());
            action.activate(Some(&target));
        };

        let manual_source_id = crate::architecture::SourceId::random();
        let manual_a = SourceObject::manual(
            "Manual A",
            "subsonic",
            "https://a.example",
            manual_source_id,
        );
        activate(Some(&manual_a));
        assert_eq!(delete_rx.try_recv().unwrap(), manual_source_id.to_string());
        assert_empty(&delete_rx);
        assert_empty(&disconnect_rx);

        // Forced remove/reinsert first runs unbind. Activating while unbound
        // cannot invoke the source installed by the previous binding.
        activate(None);
        assert_empty(&delete_rx);
        assert_empty(&disconnect_rx);

        // Reinsert the same object in its transient connecting state. The
        // stale delete action must still be absent.
        manual_a.set_connecting(true);
        activate(Some(&manual_a));
        assert_empty(&delete_rx);
        assert_empty(&disconnect_rx);

        // A second forced reinsert of the same actionable source must still
        // produce one delete, not one per historical bind.
        activate(None);
        manual_a.set_connecting(false);
        activate(Some(&manual_a));
        assert_eq!(delete_rx.try_recv().unwrap(), manual_source_id.to_string());
        assert_empty(&delete_rx);
        assert_empty(&disconnect_rx);

        // Recycle the item for a different connected DAAP source. Only that
        // source's eject event is emitted; Manual A is never deleted again.
        let daap_b = SourceObject::discovered("DAAP B", "daap", "http://b.example:3689");
        let daap_source_id = daap_b.source_id().expect("derived DAAP source ID");
        daap_b.set_connected(true);
        activate(Some(&daap_b));
        assert_eq!(
            disconnect_rx.try_recv().unwrap(),
            daap_source_id.to_string()
        );
        assert_empty(&disconnect_rx);
        assert_empty(&delete_rx);

        // Recycle once more for the Playlists header. The click opens its
        // menu and emits no server action.
        activate(Some(&SourceObject::header(
            "Localized playlist heading",
            HeaderKind::Playlists,
        )));
        assert_eq!(opened.get(), 1);
        assert_empty(&disconnect_rx);
        assert_empty(&delete_rx);
    }

    #[test]
    fn playlist_creation_actions_emit_exactly_once() {
        let (tx, rx) = async_channel::unbounded();
        let group = playlist_creation_action_group(&tx);

        group.activate_action("create-regular", None);
        assert_eq!(rx.try_recv().unwrap(), PlaylistAction::CreateRegular);
        assert_empty(&rx);

        group.activate_action("create-smart", None);
        assert_eq!(rx.try_recv().unwrap(), PlaylistAction::CreateSmart);
        assert_empty(&rx);

        group.activate_action("import", None);
        assert_eq!(rx.try_recv().unwrap(), PlaylistAction::ImportPlaylist);
        assert_empty(&rx);

        group.activate_action("browse-server-playlists", None);
        assert_eq!(
            rx.try_recv().unwrap(),
            PlaylistAction::BrowseServerPlaylists
        );
        assert_empty(&rx);
    }

    #[test]
    fn playlist_creation_menu_exposes_server_playlist_browser_action() {
        let menu = playlist_creation_menu();
        assert_eq!(menu.n_items(), 4);

        let action = menu
            .item_attribute_value(3, "action", Some(glib::VariantTy::STRING))
            .expect("server playlist browser menu item action");
        assert_eq!(action.str(), Some("pl-add.browse-server-playlists"));

        let label = menu
            .item_attribute_value(3, "label", Some(glib::VariantTy::STRING))
            .expect("server playlist browser menu item label");
        assert_eq!(
            label.str(),
            Some(rust_i18n::t!("server_playlists.browse_menu").as_ref())
        );
    }

    #[test]
    fn playlist_popup_actions_are_popover_owned_immutable_snapshots() {
        assert_eq!(
            PLAYLIST_POPUP_ACTION_OWNER,
            PlaylistPopupActionOwner::Popover
        );

        let (tx, rx) = async_channel::unbounded();
        let first_popup = playlist_popup_action_group(&tx, Some("first-playlist"));
        let rebound_popup = playlist_popup_action_group(&tx, Some("rebound-playlist"));

        // Creating the action snapshot for a rebound row cannot retarget the
        // already-open popover's immutable snapshot.
        rebound_popup.activate_action("rename", None);
        assert_eq!(
            rx.try_recv().unwrap(),
            PlaylistAction::Rename("rebound-playlist".to_string())
        );
        first_popup.activate_action("rename", None);
        assert_eq!(
            rx.try_recv().unwrap(),
            PlaylistAction::Rename("first-playlist".to_string())
        );
        assert_empty(&rx);
    }
}
