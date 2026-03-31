//! Sidebar — `GtkListView` of media backend sources, grouped by category.

use gtk::gio;
use gtk::prelude::*;

use super::objects::SourceObject;
use tracing::info;

/// Build the source sidebar.  Returns a `gtk::ScrolledWindow`.
pub fn build_sidebar(sources: &[SourceObject]) -> gtk::ScrolledWindow {
    let store = gio::ListStore::new::<SourceObject>();
    for src in sources {
        store.append(src);
    }

    let selection = gtk::SingleSelection::new(Some(store));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    // Skip the "Local" header row — select the first actual source.
    selection.set_selected(1);

    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
        let list_item = list_item
            .downcast_ref::<gtk::ListItem>()
            .expect("ListItem expected");
        // Create a flexible container
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(8)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let icon = gtk::Image::builder().pixel_size(16).build();
        let label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();

        row_box.append(&icon);
        row_box.append(&label);
        list_item.set_child(Some(&row_box));
    });

    factory.connect_bind(|_, list_item| {
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
        let label = icon
            .next_sibling()
            .and_downcast::<gtk::Label>()
            .expect("Label expected");

        if obj.is_header() {
            icon.set_visible(false);
            label.set_text(&obj.name());
            label.add_css_class("heading");
            label.add_css_class("dim-label");
            label.set_margin_top(8);
            label.set_ellipsize(gtk::pango::EllipsizeMode::None);
            list_item.set_activatable(false);
            list_item.set_selectable(false);
        } else {
            icon.set_visible(true);
            icon.set_icon_name(Some(&obj.icon_name()));
            label.set_text(&obj.name());
            label.remove_css_class("heading");
            label.remove_css_class("dim-label");
            label.set_margin_top(0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            list_item.set_activatable(true);
            list_item.set_selectable(true);
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

    gtk::ScrolledWindow::builder()
        .child(&list_view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .width_request(180)
        .build()
}
