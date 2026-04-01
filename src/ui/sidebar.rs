//! Sidebar — `GtkListView` of media backend sources, grouped by category.
//!
//! The sidebar is driven by a `gio::ListStore<SourceObject>` that can be
//! mutated at runtime (e.g., to add mDNS-discovered servers).
//!
//! DAAP sources get a monochrome eject button for disconnecting.

use gtk::gio;
use gtk::prelude::*;

use super::objects::SourceObject;
use tracing::info;

/// Build the source sidebar.
///
/// Returns `(ScrolledWindow, ListStore, SingleSelection, disconnect_rx)`.
/// The `disconnect_rx` emits the `server_url` of a DAAP source when the
/// user clicks its eject button.
pub fn build_sidebar(
    initial_sources: &[SourceObject],
) -> (
    gtk::ScrolledWindow,
    gio::ListStore,
    gtk::SingleSelection,
    async_channel::Receiver<String>,
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

    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
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
        let eject_btn = gtk::Button::builder()
            .icon_name("media-eject-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Disconnect")
            .visible(false)
            .build();

        row_box.append(&icon);
        row_box.append(&spinner);
        row_box.append(&label);
        row_box.append(&eject_btn);
        list_item.set_child(Some(&row_box));
    });

    {
        let disconnect_tx = disconnect_tx.clone();
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
            let eject_btn = label
                .next_sibling()
                .and_downcast::<gtk::Button>()
                .expect("Button expected");

            if obj.is_header() {
                icon.set_visible(false);
                spinner.set_visible(false);
                eject_btn.set_visible(false);
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
                    eject_btn.set_visible(false);
                    label.add_css_class("dim-label");
                } else if !obj.connected() && !obj.server_url().is_empty() {
                    // Discovered but not yet authenticated.
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
                    eject_btn.set_visible(false);
                } else {
                    // Connected or local source — normal icon.
                    icon.set_visible(true);
                    icon.set_icon_name(Some(&obj.icon_name()));
                    spinner.set_visible(false);
                    label.remove_css_class("dim-label");

                    // Show eject button only for connected DAAP sources.
                    if obj.backend_type() == "daap" && obj.connected() {
                        eject_btn.set_visible(true);
                        let tx = disconnect_tx.clone();
                        let source_key = obj.server_url();
                        eject_btn.connect_clicked(move |_| {
                            let _ = tx.try_send(source_key.clone());
                        });
                    } else {
                        eject_btn.set_visible(false);
                    }
                }
            }
        });
    }

    // Unbind: hide the eject button to prevent signal accumulation
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
                    info!(
                        source = %src.name(),
                        backend = %src.backend_type(),
                        connected = src.connected(),
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
        .build();

    (scrolled, store, selection, disconnect_rx)
}
