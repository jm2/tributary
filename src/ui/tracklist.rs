//! Tracklist — iTunes-style dense metadata grid using `GtkColumnView`.
//!
//! 12 resizable, sortable columns backed by `gio::ListStore<TrackObject>`
//! wrapped in a `gtk::SortListModel` for click-to-sort column headers.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use gtk::gio;
use gtk::prelude::*;

use super::objects::TrackObject;

/// Build the tracklist view.
///
/// Returns `(outer_box, track_store, status_label, column_view, sort_model)`.
/// The caller uses `track_store` for mutation (add/remove) and `sort_model`
/// for position lookups (playback, next/prev) since positions in the
/// ColumnView correspond to the sorted order, not the raw store order.
pub fn build_tracklist(
    initial_tracks: &[TrackObject],
) -> (
    gtk::Box,
    gio::ListStore,
    gtk::Label,
    gtk::ColumnView,
    gtk::SortListModel,
) {
    let store = gio::ListStore::new::<TrackObject>();
    for t in initial_tracks {
        store.append(t);
    }

    // Wrap store in a sort model so column header clicks sort the view.
    let sort_model = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
    let selection = gtk::MultiSelection::new(Some(sort_model.clone()));

    let column_view = gtk::ColumnView::builder()
        .model(&selection)
        .reorderable(true)
        .show_column_separators(true)
        .show_row_separators(true)
        .css_classes(["data-table"])
        .hexpand(true)
        .vexpand(true)
        .build();

    // ── Define columns (display getter + sort key) ──────────────────
    add_sorted_column(
        &column_view,
        "#",
        50,
        true,
        |t: &TrackObject| t.track_number().to_string(),
        |a, b| a.track_number().cmp(&b.track_number()),
    );
    add_sorted_column(
        &column_view,
        "Title",
        250,
        false,
        |t: &TrackObject| t.title(),
        |a, b| a.title().to_lowercase().cmp(&b.title().to_lowercase()),
    );
    add_sorted_column(
        &column_view,
        "Time",
        60,
        true,
        |t: &TrackObject| t.duration_display(),
        |a, b| a.duration_secs().cmp(&b.duration_secs()),
    );
    add_sorted_column(
        &column_view,
        "Artist",
        180,
        false,
        |t: &TrackObject| t.artist(),
        |a, b| a.artist().to_lowercase().cmp(&b.artist().to_lowercase()),
    );
    add_sorted_column(
        &column_view,
        "Album",
        200,
        false,
        |t: &TrackObject| t.album(),
        |a, b| a.album().to_lowercase().cmp(&b.album().to_lowercase()),
    );
    add_sorted_column(
        &column_view,
        "Genre",
        100,
        false,
        |t: &TrackObject| t.genre(),
        |a, b| a.genre().to_lowercase().cmp(&b.genre().to_lowercase()),
    );
    add_sorted_column(
        &column_view,
        "Year",
        60,
        true,
        |t: &TrackObject| t.year_display(),
        |a, b| a.year().cmp(&b.year()),
    );
    add_sorted_column(
        &column_view,
        "Date Modified",
        110,
        false,
        |t: &TrackObject| t.date_modified(),
        |a, b| a.date_modified().cmp(&b.date_modified()),
    );
    add_sorted_column(
        &column_view,
        "Bitrate",
        80,
        true,
        |t: &TrackObject| t.bitrate_display(),
        |a, b| a.bitrate_kbps().cmp(&b.bitrate_kbps()),
    );
    add_sorted_column(
        &column_view,
        "Sample Rate",
        80,
        true,
        |t: &TrackObject| t.sample_rate_display(),
        |a, b| a.sample_rate_hz().cmp(&b.sample_rate_hz()),
    );
    add_sorted_column(
        &column_view,
        "Plays",
        60,
        true,
        |t: &TrackObject| t.play_count_display(),
        |a, b| a.play_count().cmp(&b.play_count()),
    );
    add_sorted_column(
        &column_view,
        "Format",
        60,
        false,
        |t: &TrackObject| t.format(),
        |a, b| a.format().cmp(&b.format()),
    );

    // Connect the ColumnView's composite sorter to the SortListModel
    // so that clicking headers actually re-orders the rows.
    sort_model.set_sorter(column_view.sorter().as_ref());

    // ── Three-state sort: asc → desc → none ─────────────────────────
    // GTK4 only cycles asc↔desc. We intercept to add a third "none"
    // state: if the user clicks a column that is already descending,
    // clear the sort entirely to restore the original insertion order.
    if let Some(sorter) = column_view.sorter() {
        let cv = column_view.clone();
        let sm = sort_model.clone();
        // Track (column_title, was_descending) from the previous state.
        let prev: Rc<RefCell<Option<(String, bool)>>> = Rc::new(RefCell::new(None));

        sorter.connect_changed(move |_, _| {
            let Some(cv_sorter) = cv.sorter() else { return };
            let Some(cv_sorter) = cv_sorter.downcast_ref::<gtk::ColumnViewSorter>() else {
                return;
            };
            let Some(col) = cv_sorter.primary_sort_column() else {
                // Already cleared.
                *prev.borrow_mut() = None;
                return;
            };
            let title = col.title().map(|t| t.to_string()).unwrap_or_default();
            let is_desc = cv_sorter.primary_sort_order() == gtk::SortType::Descending;

            let mut prev = prev.borrow_mut();
            if let Some((ref prev_title, prev_desc)) = *prev {
                if *prev_title == title && prev_desc && !is_desc {
                    // Column flipped from desc back to asc — that means
                    // the user clicked it a third time.  Clear sorting.
                    // Use idle_add_local_once to avoid re-entrant sorter mutation.
                    let cv2 = cv.clone();
                    let sm2 = sm.clone();
                    gtk::glib::idle_add_local_once(move || {
                        cv2.sort_by_column(
                            None::<&gtk::ColumnViewColumn>,
                            gtk::SortType::Ascending,
                        );
                        sm2.set_sorter(cv2.sorter().as_ref());
                    });
                }
            }
            *prev = Some((title, is_desc));
        });
    }

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

    (outer, store, status_label, column_view, sort_model)
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

fn add_sorted_column<F, S>(
    column_view: &gtk::ColumnView,
    title: &str,
    fixed_width: i32,
    right_align: bool,
    getter: F,
    sort_fn: S,
) where
    F: Fn(&TrackObject) -> String + 'static,
    S: Fn(&TrackObject, &TrackObject) -> Ordering + 'static,
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

    let sorter = gtk::CustomSorter::new(move |a, b| {
        let ta = a.downcast_ref::<TrackObject>().unwrap();
        let tb = b.downcast_ref::<TrackObject>().unwrap();
        sort_fn(ta, tb).into()
    });

    let column = gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .resizable(true)
        .fixed_width(fixed_width)
        .build();

    column_view.append_column(&column);
}
