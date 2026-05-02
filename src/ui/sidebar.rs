//! Sidebar — `GtkListView` of media backend sources, grouped by category.
//!
//! The sidebar is driven by a `gio::ListStore<SourceObject>` that can be
//! mutated at runtime (e.g., to add mDNS-discovered servers).
//!
//! DAAP sources get a monochrome eject button for disconnecting.
//! Manually-added servers get a trash button for removal.

use gtk::gio;
use gtk::prelude::*;

use super::objects::SourceObject;
use tracing::debug;

/// Playlist action emitted from the sidebar context menu.
#[derive(Debug, Clone)]
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
    /// Export a playlist to an XSPF file (id).
    ExportPlaylist(String),
}

/// Build the source sidebar.
///
/// Returns `(sidebar_box, ListStore, SingleSelection, disconnect_rx, delete_rx, add_button, playlist_action_rx)`.
///
/// * `disconnect_rx` emits the `server_url` of a DAAP source when the
///   user clicks its eject button.
/// * `delete_rx` emits the `server_url` of a manually-added source when
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
            // Action button: eject (DAAP) or trash (manual) — reused widget.
            let action_btn = gtk::Button::builder()
                .css_classes(["flat", "circular"])
                .visible(false)
                .build();

            row_box.append(&icon);
            row_box.append(&spinner);
            row_box.append(&label);
            row_box.append(&action_btn);
            list_item.set_child(Some(&row_box));

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

                let bt = src.backend_type();
                let is_playlist = bt == "playlist" || bt == "smart-playlist";
                let is_playlist_header = src.is_header() && src.name() == "Playlists";
                if !is_playlist && !is_playlist_header {
                    return;
                }

                let menu = gtk::gio::Menu::new();
                if is_playlist_header {
                    menu.append(Some("New Playlist"), Some("playlist.create-regular"));
                    menu.append(Some("New Smart Playlist"), Some("playlist.create-smart"));
                    menu.append(Some("Import Playlist\u{2026}"), Some("playlist.import"));
                } else if is_playlist {
                    menu.append(Some("Rename"), Some("playlist.rename"));
                    menu.append(Some("Export\u{2026}"), Some("playlist.export"));
                    menu.append(Some("Delete"), Some("playlist.delete"));
                    if bt == "smart-playlist" {
                        menu.append(
                            Some("Edit Smart Playlist\u{2026}"),
                            Some("playlist.edit-smart"),
                        );
                    }
                }

                let action_group = gtk::gio::SimpleActionGroup::new();
                let pid = src.playlist_id();

                let tx = tx_for_gesture.clone();
                let create_reg = gtk::gio::SimpleAction::new("create-regular", None);
                create_reg.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::CreateRegular);
                });
                action_group.add_action(&create_reg);

                let tx = tx_for_gesture.clone();
                let create_smart = gtk::gio::SimpleAction::new("create-smart", None);
                create_smart.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::CreateSmart);
                });
                action_group.add_action(&create_smart);

                let tx = tx_for_gesture.clone();
                let pid_clone = pid.clone();
                let rename = gtk::gio::SimpleAction::new("rename", None);
                rename.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::Rename(pid_clone.clone()));
                });
                action_group.add_action(&rename);

                let tx = tx_for_gesture.clone();
                let pid_clone = pid.clone();
                let delete = gtk::gio::SimpleAction::new("delete", None);
                delete.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::Delete(pid_clone.clone()));
                });
                action_group.add_action(&delete);

                let tx = tx_for_gesture.clone();
                let pid_clone = pid.clone();
                let edit_smart = gtk::gio::SimpleAction::new("edit-smart", None);
                edit_smart.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::EditSmart(pid_clone.clone()));
                });
                action_group.add_action(&edit_smart);

                let tx = tx_for_gesture.clone();
                let import = gtk::gio::SimpleAction::new("import", None);
                import.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::ImportPlaylist);
                });
                action_group.add_action(&import);

                let tx = tx_for_gesture.clone();
                let pid_clone = pid.clone();
                let export = gtk::gio::SimpleAction::new("export", None);
                export.connect_activate(move |_, _| {
                    let _ = tx.try_send(PlaylistAction::ExportPlaylist(pid_clone.clone()));
                });
                action_group.add_action(&export);

                row_box_for_gesture.insert_action_group("playlist", Some(&action_group));

                let popover = gtk::PopoverMenu::from_model(Some(&menu));
                popover.set_parent(&row_box_for_gesture);
                #[allow(clippy::cast_possible_truncation)]
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                    x as i32, y as i32, 1, 1,
                )));
                popover.popup();
            });
            row_box.add_controller(gesture);
        });
    }

    {
        let disconnect_tx = disconnect_tx.clone();
        let delete_tx = delete_tx.clone();
        let playlist_action_tx = playlist_action_tx.clone();
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

                // Show a "+" button on the Playlists header for creating
                // new playlists (most discoverable entry point).
                if obj.name() == "Playlists" {
                    action_btn.set_icon_name("list-add-symbolic");
                    action_btn.set_tooltip_text(Some("New playlist"));
                    action_btn.set_visible(true);
                    let tx = playlist_action_tx.clone();
                    action_btn.connect_clicked(move |btn| {
                        // Build a small popover menu with playlist actions.
                        let menu = gtk::gio::Menu::new();
                        menu.append(Some("New Playlist"), Some("pl-add.create-regular"));
                        menu.append(Some("New Smart Playlist"), Some("pl-add.create-smart"));
                        menu.append(
                            Some("Import Playlist\u{2026}"),
                            Some("pl-add.import"),
                        );

                        let ag = gtk::gio::SimpleActionGroup::new();

                        let tx_reg = tx.clone();
                        let reg = gtk::gio::SimpleAction::new("create-regular", None);
                        reg.connect_activate(move |_, _| {
                            let _ = tx_reg.try_send(PlaylistAction::CreateRegular);
                        });
                        ag.add_action(&reg);

                        let tx_smart = tx.clone();
                        let smart = gtk::gio::SimpleAction::new("create-smart", None);
                        smart.connect_activate(move |_, _| {
                            let _ = tx_smart.try_send(PlaylistAction::CreateSmart);
                        });
                        ag.add_action(&smart);

                        let tx_import = tx.clone();
                        let import = gtk::gio::SimpleAction::new("import", None);
                        import.connect_activate(move |_, _| {
                            let _ = tx_import.try_send(PlaylistAction::ImportPlaylist);
                        });
                        ag.add_action(&import);

                        btn.insert_action_group("pl-add", Some(&ag));

                        let popover = gtk::PopoverMenu::from_model(Some(&menu));
                        popover.set_parent(btn);
                        popover.popup();
                    });
                } else {
                    action_btn.set_visible(false);
                }
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
                    action_btn.set_visible(false);
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

                    // Show trash button for manually-added (disconnected) servers.
                    if obj.manually_added() {
                        action_btn.set_icon_name("user-trash-symbolic");
                        action_btn.set_tooltip_text(Some("Remove server"));
                        action_btn.set_visible(true);
                        let tx = delete_tx.clone();
                        let source_key = obj.server_url();
                        action_btn.connect_clicked(move |_| {
                            let _ = tx.try_send(source_key.clone());
                        });
                    } else {
                        action_btn.set_visible(false);
                    }
                } else {
                    // Connected or local source — normal icon.
                    icon.set_visible(true);
                    icon.set_icon_name(Some(&obj.icon_name()));
                    spinner.set_visible(false);
                    label.remove_css_class("dim-label");

                    if obj.backend_type() == "daap" && obj.connected() {
                        // Show eject button for connected DAAP sources.
                        action_btn.set_icon_name("media-eject-symbolic");
                        action_btn.set_tooltip_text(Some("Disconnect"));
                        action_btn.set_visible(true);
                        let tx = disconnect_tx.clone();
                        let source_key = obj.server_url();
                        action_btn.connect_clicked(move |_| {
                            let _ = tx.try_send(source_key.clone());
                        });
                    } else if obj.manually_added() && obj.connected() {
                        // Show trash button for connected manually-added servers.
                        action_btn.set_icon_name("user-trash-symbolic");
                        action_btn.set_tooltip_text(Some("Remove server"));
                        action_btn.set_visible(true);
                        let tx = delete_tx.clone();
                        let source_key = obj.server_url();
                        action_btn.connect_clicked(move |_| {
                            let _ = tx.try_send(source_key.clone());
                        });
                    } else {
                        action_btn.set_visible(false);
                    }
                }
            }
        });
    }

    // Unbind: hide the action button to prevent signal accumulation
    // when list items are recycled.
    factory.connect_unbind(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");
        if let Some(row_box) = list_item.child().and_downcast::<gtk::Box>() {
            if let Some(btn) = row_box.last_child().and_downcast::<gtk::Button>() {
                btn.set_visible(false);
            }
        }
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
