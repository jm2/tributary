//! Tracklist — iTunes-style dense metadata grid using `GtkColumnView`.
//!
//! 14 resizable, sortable columns backed by `gio::ListStore<TrackObject>`
//! wrapped in a `gtk::SortListModel` for click-to-sort column headers.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use gtk::gio;
use gtk::prelude::*;

use crate::architecture::models::{Rating, RatingCapability, TrackRating};
use crate::architecture::{SourceId, TrackId};
use crate::local::engine::LibraryCommand;

use super::library_commands::LibraryCommandAdmission;
use super::objects::{PlaylistOccurrenceState, TrackObject};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RatingCellPresentation {
    text: String,
    accessible_label: String,
    editable: bool,
    input_value: u8,
}

/// Build the tracklist view.
///
/// Returns `(outer_box, track_store, status_label, column_view, sort_model)`.
/// The caller uses `track_store` for mutation (add/remove) and `sort_model`
/// for position lookups (playback, next/prev) since positions in the
/// ColumnView correspond to the sorted order, not the raw store order.
pub(super) fn build_tracklist(
    initial_tracks: &[TrackObject],
    library_commands: LibraryCommandAdmission,
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
        "Composer",
        140,
        false,
        |t: &TrackObject| t.composer(),
        |a, b| {
            a.composer()
                .to_lowercase()
                .cmp(&b.composer().to_lowercase())
        },
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
    add_rating_column(&column_view, library_commands);
    add_sorted_column(
        &column_view,
        "Format",
        60,
        false,
        |t: &TrackObject| t.format(),
        |a, b| a.format().cmp(&b.format()),
    );

    // ── Sentinel (spacer) column ─────────────────────────────────────
    // GTK4 auto-expands the rightmost column to fill remaining space,
    // making it impossible for users to resize by dragging its right
    // edge.  This invisible zero-width sentinel absorbs the expansion
    // so every real column keeps a draggable resize handle.  (#12)
    {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
            list_item.set_child(Some(&gtk::Label::new(None)));
        });
        let sentinel = gtk::ColumnViewColumn::builder()
            .title("")
            .factory(&factory)
            .resizable(false)
            .fixed_width(0)
            .build();
        column_view.append_column(&sentinel);
    }

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
        if matches!(
            track
                .playlist_occurrence_binding()
                .map(|binding| binding.state()),
            Some(PlaylistOccurrenceState::Unavailable(_))
        ) {
            let accessible = format!("{} — {}", track.title(), track.artist());
            label.add_css_class("dim-label");
            label.set_tooltip_text(Some(&accessible));
            label.update_property(&[gtk::accessible::Property::Label(&accessible)]);
        } else {
            label.remove_css_class("dim-label");
            label.set_tooltip_text(None);
            label.reset_property(gtk::AccessibleProperty::Label);
        }
    });

    // Clear label text on recycle to prevent stale data from appearing
    // in the wrong row during rapid scrolling (GTK4 recycling issue).
    factory.connect_unbind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        if let Some(label) = list_item.child().and_downcast::<gtk::Label>() {
            label.set_text("");
            label.remove_css_class("dim-label");
            label.set_tooltip_text(None);
            label.reset_property(gtk::AccessibleProperty::Label);
        }
    });

    let sorter = gtk::CustomSorter::new(move |a, b| {
        let ta = a
            .downcast_ref::<TrackObject>()
            .expect("sort model contains TrackObject");
        let tb = b
            .downcast_ref::<TrackObject>()
            .expect("sort model contains TrackObject");
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

fn add_rating_column(column_view: &gtk::ColumnView, library_commands: LibraryCommandAdmission) {
    let factory = gtk::SignalListItemFactory::new();
    let setup_commands = library_commands.clone();

    factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");

        let input_label = gtk::Label::builder()
            .label(rust_i18n::t!("ratings.input_label").as_ref())
            .halign(gtk::Align::Start)
            .build();
        let spin = gtk::SpinButton::with_range(1.0, 100.0, 1.0);
        spin.set_numeric(true);
        spin.set_value(100.0);
        spin.update_property(&[gtk::accessible::Property::Label(
            rust_i18n::t!("ratings.input_label").as_ref(),
        )]);

        let apply = gtk::Button::builder()
            .label(rust_i18n::t!("ratings.apply").as_ref())
            .build();
        let clear = gtk::Button::builder()
            .label(rust_i18n::t!("ratings.clear").as_ref())
            .build();
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::End)
            .build();
        actions.append(&clear);
        actions.append(&apply);

        let editor = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();
        editor.append(&input_label);
        editor.append(&spin);
        editor.append(&actions);

        let popover = gtk::Popover::builder().child(&editor).build();
        let button = gtk::MenuButton::builder()
            .popover(&popover)
            .css_classes(["flat"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .margin_start(2)
            .margin_end(2)
            .build();
        button.set_sensitive(false);
        list_item.set_child(Some(&button));

        let apply_item = list_item.downgrade();
        let apply_spin = spin.downgrade();
        let apply_button = button.downgrade();
        let apply_commands = setup_commands.clone();
        apply.connect_clicked(move |_| {
            let (Some(list_item), Some(spin), Some(button)) = (
                apply_item.upgrade(),
                apply_spin.upgrade(),
                apply_button.upgrade(),
            ) else {
                return;
            };
            let Ok(rating) = Rating::try_from(spin.value_as_int()) else {
                return;
            };
            if queue_rating_command(&list_item, &apply_commands, Some(rating)) {
                button.popdown();
            }
        });

        let clear_item = list_item.downgrade();
        let clear_button = button.downgrade();
        let clear_commands = setup_commands.clone();
        clear.connect_clicked(move |_| {
            let (Some(list_item), Some(button)) = (clear_item.upgrade(), clear_button.upgrade())
            else {
                return;
            };
            if queue_rating_command(&list_item, &clear_commands, None) {
                button.popdown();
            }
        });
    });

    factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let track = list_item
            .item()
            .and_downcast::<TrackObject>()
            .expect("TrackObject");
        let button = list_item
            .child()
            .and_downcast::<gtk::MenuButton>()
            .expect("rating MenuButton");
        let presentation = rating_cell_presentation(track.rating(), &rust_i18n::locale());
        let unavailable = matches!(
            track
                .playlist_occurrence_binding()
                .map(|binding| binding.state()),
            Some(PlaylistOccurrenceState::Unavailable(_))
        );
        let accessible_label = if unavailable {
            format!("{} — {}", track.title(), track.artist())
        } else {
            presentation.accessible_label.clone()
        };
        button.set_label(&presentation.text);
        button.set_sensitive(presentation.editable && local_rating_track_id(&track).is_some());
        button.set_tooltip_text(Some(&accessible_label));
        button.update_property(&[gtk::accessible::Property::Label(&accessible_label)]);
        if unavailable {
            button.add_css_class("dim-label");
        } else {
            button.remove_css_class("dim-label");
        }

        if let Some(spin) = rating_spin_button(&button) {
            spin.set_value(f64::from(presentation.input_value));
        }
    });

    factory.connect_unbind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        if let Some(button) = list_item.child().and_downcast::<gtk::MenuButton>() {
            button.popdown();
            button.set_sensitive(false);
            button.set_label("");
            button.set_tooltip_text(None);
            button.remove_css_class("dim-label");
            button.reset_property(gtk::AccessibleProperty::Label);
            if let Some(spin) = rating_spin_button(&button) {
                spin.set_value(100.0);
            }
        }
    });

    let rating_title = rust_i18n::t!("columns.rating").into_owned();
    let sort_rating_title = rating_title.clone();
    let column_view_weak = column_view.downgrade();
    let sorter = gtk::CustomSorter::new(move |a, b| {
        let first = a
            .downcast_ref::<TrackObject>()
            .expect("sort model contains TrackObject");
        let second = b
            .downcast_ref::<TrackObject>()
            .expect("sort model contains TrackObject");
        let descending = column_view_weak.upgrade().is_some_and(|view| {
            view.sorter()
                .and_downcast::<gtk::ColumnViewSorter>()
                .is_some_and(|sorter| {
                    rating_sort_is_descending(
                        (0..sorter.n_sort_columns()).filter_map(|index| {
                            let (column, order) = sorter.nth_sort_column(index);
                            Some((column?.title()?, order))
                        }),
                        &sort_rating_title,
                    )
                })
        });
        compare_rating_rows(first, second, descending).into()
    });

    let column = gtk::ColumnViewColumn::builder()
        .title(&rating_title)
        .factory(&factory)
        .sorter(&sorter)
        .resizable(true)
        .fixed_width(120)
        .build();
    column_view.append_column(&column);
}

fn rating_spin_button(button: &gtk::MenuButton) -> Option<gtk::SpinButton> {
    button
        .popover()
        .and_downcast::<gtk::Popover>()?
        .child()
        .and_downcast::<gtk::Box>()?
        .first_child()?
        .next_sibling()?
        .downcast::<gtk::SpinButton>()
        .ok()
}

fn queue_rating_command(
    list_item: &gtk::ListItem,
    commands: &LibraryCommandAdmission,
    rating: Option<Rating>,
) -> bool {
    let Some(track) = list_item.item().and_downcast::<TrackObject>() else {
        return false;
    };
    let Some(track_id) = local_rating_track_id(&track) else {
        return false;
    };
    commands.try_send(LibraryCommand::SetTrackRating { track_id, rating })
}

fn local_rating_track_id(track: &TrackObject) -> Option<TrackId> {
    if track.source_id() != Some(SourceId::local()) {
        return None;
    }
    if !matches!(track.rating(), TrackRating::Writable { .. }) {
        return None;
    }
    if track
        .playlist_occurrence_binding()
        .is_some_and(|binding| binding.state() != PlaylistOccurrenceState::AvailableLocal)
    {
        return None;
    }
    TrackId::new(track.track_id()).ok()
}

fn rating_cell_presentation(rating: TrackRating, locale: &str) -> RatingCellPresentation {
    let input_value = rating.value().map_or(100, Rating::value);
    match rating {
        TrackRating::Writable { value: Some(value) } => RatingCellPresentation {
            text: value.value().to_string(),
            accessible_label: rust_i18n::t!(
                "ratings.edit_value",
                locale = locale,
                value = value.value()
            )
            .into_owned(),
            editable: true,
            input_value,
        },
        TrackRating::Writable { value: None } => RatingCellPresentation {
            text: rust_i18n::t!("ratings.unrated", locale = locale).into_owned(),
            accessible_label: rust_i18n::t!("ratings.edit_unrated", locale = locale).into_owned(),
            editable: true,
            input_value,
        },
        TrackRating::ReadOnly { value: Some(value) } => RatingCellPresentation {
            text: rust_i18n::t!(
                "ratings.read_only_value",
                locale = locale,
                value = value.value()
            )
            .into_owned(),
            accessible_label: rust_i18n::t!(
                "ratings.read_only_value",
                locale = locale,
                value = value.value()
            )
            .into_owned(),
            editable: false,
            input_value,
        },
        TrackRating::ReadOnly { value: None } => RatingCellPresentation {
            text: rust_i18n::t!("ratings.read_only_unrated", locale = locale).into_owned(),
            accessible_label: rust_i18n::t!("ratings.read_only_unrated", locale = locale)
                .into_owned(),
            editable: false,
            input_value,
        },
        TrackRating::Unsupported => RatingCellPresentation {
            text: rust_i18n::t!("ratings.unavailable", locale = locale).into_owned(),
            accessible_label: rust_i18n::t!("ratings.unavailable", locale = locale).into_owned(),
            editable: false,
            input_value,
        },
    }
}

fn compare_rating_rows(first: &TrackObject, second: &TrackObject, descending: bool) -> Ordering {
    let first_rating = first.rating();
    let second_rating = second.rating();
    let first_category = rating_sort_category(first_rating);
    let second_category = rating_sort_category(second_rating);
    let category_order = first_category.cmp(&second_category);
    if category_order != Ordering::Equal {
        return if descending {
            category_order.reverse()
        } else {
            category_order
        };
    }

    first_rating
        .value()
        .map(Rating::value)
        .cmp(&second_rating.value().map(Rating::value))
        .then_with(|| first.track_id().cmp(&second.track_id()))
}

fn rating_sort_category(rating: TrackRating) -> u8 {
    if rating.value().is_some() {
        0
    } else if rating.capability() == RatingCapability::Unsupported {
        2
    } else {
        1
    }
}

/// Return the direction assigned specifically to Rating, including when GTK
/// uses it as a secondary compound-sort key.
fn rating_sort_is_descending<I, S>(columns: I, rating_title: &str) -> bool
where
    I: IntoIterator<Item = (S, gtk::SortType)>,
    S: AsRef<str>,
{
    columns
        .into_iter()
        .any(|(title, order)| title.as_ref() == rating_title && order == gtk::SortType::Descending)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct RatingCatalog {
        columns: RatingColumns,
        ratings: RatingMessages,
    }

    #[derive(Debug, Deserialize)]
    struct RatingColumns {
        rating: String,
    }

    #[derive(Debug, Deserialize)]
    struct RatingMessages {
        unrated: String,
        read_only_value: String,
        read_only_unrated: String,
        unavailable: String,
        edit_value: String,
        edit_unrated: String,
        input_label: String,
        apply: String,
        clear: String,
        update_failed: String,
    }

    fn track(id: &str, rating: TrackRating) -> TrackObject {
        let track = TrackObject::new(
            1, "Title", 60, "Artist", "Album", "", "", 0, "", 0, 0, 0, "", "",
        );
        track.set_track_id(id);
        track.set_rating(rating);
        track
    }

    fn final_rating_order(first: &TrackObject, second: &TrackObject, descending: bool) -> Ordering {
        let order = compare_rating_rows(first, second, descending);
        if descending {
            order.reverse()
        } else {
            order
        }
    }

    #[test]
    fn rating_presentations_are_honest_and_accessible_in_every_locale() {
        let value = Rating::new(73).unwrap();
        let locale_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");
        let value_placeholder = ["%", "{", "value", "}"].concat();
        for locale in rust_i18n::available_locales!() {
            let path = locale_dir.join(format!("{locale}.yml"));
            let yaml = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let catalog: RatingCatalog = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            let writable =
                rating_cell_presentation(TrackRating::writable(Some(value)), locale.as_ref());
            let unrated = rating_cell_presentation(TrackRating::writable(None), locale.as_ref());
            let read_only =
                rating_cell_presentation(TrackRating::read_only(Some(value)), locale.as_ref());
            let read_only_unrated =
                rating_cell_presentation(TrackRating::read_only(None), locale.as_ref());
            let unsupported = rating_cell_presentation(TrackRating::unsupported(), locale.as_ref());

            assert!(writable.editable);
            assert!(unrated.editable);
            assert!(!read_only.editable);
            assert!(!read_only_unrated.editable);
            assert!(!unsupported.editable);
            assert_eq!(writable.text, "73");
            assert_eq!(unrated.text, catalog.ratings.unrated);
            assert_eq!(
                writable.accessible_label,
                catalog.ratings.edit_value.replace(&value_placeholder, "73")
            );
            assert_eq!(unrated.accessible_label, catalog.ratings.edit_unrated);
            assert_eq!(
                read_only.text,
                catalog
                    .ratings
                    .read_only_value
                    .replace(&value_placeholder, "73")
            );
            assert_eq!(read_only_unrated.text, catalog.ratings.read_only_unrated);
            assert_eq!(unsupported.text, catalog.ratings.unavailable);
            assert_eq!(
                rust_i18n::t!("columns.rating", locale = locale).as_ref(),
                catalog.columns.rating
            );
            assert_eq!(
                rust_i18n::t!("ratings.input_label", locale = locale).as_ref(),
                catalog.ratings.input_label
            );
            assert_eq!(
                rust_i18n::t!("ratings.apply", locale = locale).as_ref(),
                catalog.ratings.apply
            );
            assert_eq!(
                rust_i18n::t!("ratings.clear", locale = locale).as_ref(),
                catalog.ratings.clear
            );
            assert_eq!(
                rust_i18n::t!("ratings.update_failed", locale = locale).as_ref(),
                catalog.ratings.update_failed
            );
            assert!(writable.accessible_label.contains("73"));
            assert!(!writable.accessible_label.contains(&value_placeholder));
            for presentation in [writable, unrated, read_only, read_only_unrated, unsupported] {
                assert!(!presentation.text.trim().is_empty(), "locale {locale}");
                assert!(
                    !presentation.accessible_label.trim().is_empty(),
                    "locale {locale}"
                );
                assert!((1..=100).contains(&presentation.input_value));
            }
        }
    }

    #[test]
    fn rating_sort_keeps_missing_values_last_in_both_directions() {
        let low = track("low", TrackRating::writable(Some(Rating::new(10).unwrap())));
        let high = track(
            "high",
            TrackRating::read_only(Some(Rating::new(90).unwrap())),
        );
        let unrated = track("unrated", TrackRating::writable(None));
        let unsupported = track("unsupported", TrackRating::unsupported());

        assert_eq!(final_rating_order(&low, &high, false), Ordering::Less);
        assert_eq!(final_rating_order(&high, &low, true), Ordering::Less);
        for descending in [false, true] {
            assert_eq!(
                final_rating_order(&low, &unrated, descending),
                Ordering::Less
            );
            assert_eq!(
                final_rating_order(&unrated, &unsupported, descending),
                Ordering::Less
            );
        }
    }

    #[test]
    fn equal_ratings_have_a_deterministic_exact_id_tie_break() {
        let rating = TrackRating::writable(Some(Rating::new(50).unwrap()));
        let first = track("a", rating);
        let second = track("b", rating);
        assert_eq!(final_rating_order(&first, &second, false), Ordering::Less);
        assert_eq!(final_rating_order(&second, &first, true), Ordering::Less);
    }

    #[test]
    fn secondary_rating_sort_uses_its_own_direction() {
        assert!(rating_sort_is_descending(
            [
                ("Artiste", gtk::SortType::Ascending),
                ("Note", gtk::SortType::Descending),
            ],
            "Note",
        ));
        assert!(!rating_sort_is_descending(
            [
                ("Artiste", gtk::SortType::Descending),
                ("Note", gtk::SortType::Ascending),
            ],
            "Note",
        ));
    }

    #[test]
    fn rating_write_requires_local_source_even_when_native_ids_and_cached_capability_collide() {
        let native_id = "shared-native-id";
        let writable = TrackRating::writable(Some(Rating::new(80).expect("rating")));
        let local = track(native_id, writable);
        assert!(local.set_source_id(SourceId::local()));
        let remote = track(native_id, writable);
        assert!(remote.set_source_id(SourceId::random()));

        assert_eq!(
            local_rating_track_id(&local).as_ref().map(TrackId::as_str),
            Some(native_id)
        );
        assert_eq!(local_rating_track_id(&remote), None);
    }

    #[test]
    fn rating_write_rejects_unavailable_or_malformed_playlist_bindings() {
        use crate::ui::objects::{PlaylistOccurrenceBinding, PlaylistRowUnavailableReason};

        let available_id = TrackId::new("available-local").expect("available track ID");
        let available = track(
            available_id.as_str(),
            TrackRating::writable(Some(Rating::new(75).expect("rating"))),
        );
        available.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::available_local("entry-available", available_id.clone())
                .expect("available binding"),
        );

        let unavailable_id = TrackId::new("missing-local").expect("missing track ID");
        let unavailable = track(
            unavailable_id.as_str(),
            TrackRating::writable(Some(Rating::new(75).expect("rating"))),
        );
        unavailable.set_playlist_occurrence_binding(
            PlaylistOccurrenceBinding::unavailable(
                "entry-unavailable",
                SourceId::local(),
                Some(unavailable_id),
                PlaylistRowUnavailableReason::LocalTrackMissing,
            )
            .expect("unavailable binding"),
        );

        let missing_source = track("missing-source", TrackRating::writable(None));
        let malformed_id = track("initially-valid", TrackRating::writable(None));
        assert!(malformed_id.set_source_id(SourceId::local()));
        malformed_id.set_track_id("");

        assert_eq!(local_rating_track_id(&available), Some(available_id));
        assert_eq!(local_rating_track_id(&unavailable), None);
        assert_eq!(local_rating_track_id(&missing_source), None);
        assert_eq!(local_rating_track_id(&malformed_id), None);
    }
}
