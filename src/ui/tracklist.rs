//! Tracklist — iTunes-style dense metadata grid using `GtkColumnView`.
//!
//! 12 resizable columns backed by `gio::ListStore<TrackObject>`.

use gtk::gio;
use gtk::prelude::*;

use super::objects::TrackObject;

/// Build the tracklist view.
///
/// Returns `(outer_box, track_store, status_label)` so the caller can
/// update the store when browser filters change and refresh the status.
pub fn build_tracklist(initial_tracks: &[TrackObject]) -> (gtk::Box, gio::ListStore, gtk::Label) {
    let store = gio::ListStore::new::<TrackObject>();
    for t in initial_tracks {
        store.append(t);
    }

    let selection = gtk::SingleSelection::new(Some(store.clone()));

    let column_view = gtk::ColumnView::builder()
        .model(&selection)
        .show_column_separators(true)
        .show_row_separators(true)
        .css_classes(["data-table"])
        .hexpand(true)
        .vexpand(true)
        .build();

    // ── Define columns ───────────────────────────────────────────────
    add_column(&column_view, "#", 50, true, |t: &TrackObject| {
        t.track_number().to_string()
    });
    add_column(&column_view, "Title", 250, false, |t: &TrackObject| {
        t.title()
    });
    add_column(&column_view, "Time", 60, true, |t: &TrackObject| {
        t.duration_display()
    });
    add_column(&column_view, "Artist", 180, false, |t: &TrackObject| {
        t.artist()
    });
    add_column(&column_view, "Album", 200, false, |t: &TrackObject| {
        t.album()
    });
    add_column(&column_view, "Genre", 100, false, |t: &TrackObject| {
        t.genre()
    });
    add_column(&column_view, "Year", 60, true, |t: &TrackObject| {
        t.year_display()
    });
    add_column(
        &column_view,
        "Date Modified",
        110,
        false,
        |t: &TrackObject| t.date_modified(),
    );
    add_column(&column_view, "Bitrate", 80, true, |t: &TrackObject| {
        t.bitrate_display()
    });
    add_column(&column_view, "Sample Rate", 80, true, |t: &TrackObject| {
        t.sample_rate_display()
    });
    add_column(&column_view, "Plays", 60, true, |t: &TrackObject| {
        t.play_count_display()
    });
    add_column(&column_view, "Format", 60, false, |t: &TrackObject| {
        t.format()
    });

    // ── Scrolled container ───────────────────────────────────────────
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&column_view)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    // ── Status bar ───────────────────────────────────────────────────
    let status_label = gtk::Label::builder()
        .halign(gtk::Align::End)
        .margin_start(8)
        .margin_end(12)
        .margin_top(4)
        .margin_bottom(4)
        .css_classes(["dim-label", "caption"])
        .build();
    update_status(&status_label, initial_tracks);

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&scrolled);
    outer.append(&status_label);

    (outer, store, status_label)
}

/// Recompute and set the status label text from the current tracks.
pub fn update_status(label: &gtk::Label, tracks: &[TrackObject]) {
    let count = tracks.len();
    let total_secs: u64 = tracks.iter().map(|t| t.duration_secs()).sum();
    let hours = total_secs as f64 / 3600.0;
    if hours >= 1.0 {
        label.set_text(&format!("{count} songs, {hours:.1} hours"));
    } else {
        let mins = total_secs as f64 / 60.0;
        label.set_text(&format!("{count} songs, {mins:.0} minutes"));
    }
}

// ---------------------------------------------------------------------------
// Column helper
// ---------------------------------------------------------------------------

fn add_column<F>(
    column_view: &gtk::ColumnView,
    title: &str,
    fixed_width: i32,
    right_align: bool,
    getter: F,
) where
    F: Fn(&TrackObject) -> String + 'static,
{
    let factory = gtk::SignalListItemFactory::new();

    let ra = right_align;
    factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let label = gtk::Label::builder()
            .halign(if ra {
                gtk::Align::End
            } else {
                gtk::Align::Start
            })
            .margin_start(6)
            .margin_end(6)
            .margin_top(2)
            .margin_bottom(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        list_item.set_child(Some(&label));
    });

    factory.connect_bind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let track = list_item
            .item()
            .and_downcast::<TrackObject>()
            .expect("TrackObject");
        let label = list_item
            .child()
            .and_downcast::<gtk::Label>()
            .expect("Label");
        label.set_text(&getter(&track));
    });

    let column = gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .resizable(true)
        .fixed_width(fixed_width)
        .build();

    column_view.append_column(&column);
}
