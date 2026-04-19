//! Smart playlist editor dialog — iTunes-style rule editor.
//!
//! Presents a modal `adw::AlertDialog` for creating and editing smart
//! playlist rules. Each rule row has field, operator, and value widgets
//! that dynamically update based on the selected field type.

use adw::prelude::*;

use crate::local::smart_rules::*;

// ── Field metadata ──────────────────────────────────────────────────

/// Field display names in dropdown order.
const FIELD_NAMES: &[&str] = &[
    "Title",
    "Artist",
    "Album Artist",
    "Album",
    "Genre",
    "Year",
    "Track Number",
    "Disc Number",
    "Duration (sec)",
    "Bitrate (kbps)",
    "Sample Rate (Hz)",
    "Format",
    "Play Count",
    "Date Added",
    "Date Modified",
    "File Size (bytes)",
];

/// Map dropdown index to `RuleField`.
fn index_to_field(idx: u32) -> RuleField {
    match idx {
        0 => RuleField::Title,
        1 => RuleField::Artist,
        2 => RuleField::AlbumArtist,
        3 => RuleField::Album,
        4 => RuleField::Genre,
        5 => RuleField::Year,
        6 => RuleField::TrackNumber,
        7 => RuleField::DiscNumber,
        8 => RuleField::Duration,
        9 => RuleField::Bitrate,
        10 => RuleField::SampleRate,
        11 => RuleField::Format,
        12 => RuleField::PlayCount,
        13 => RuleField::DateAdded,
        14 => RuleField::DateModified,
        15 => RuleField::FileSize,
        _ => RuleField::Title,
    }
}

/// Map `RuleField` to dropdown index.
fn field_to_index(field: &RuleField) -> u32 {
    match field {
        RuleField::Title => 0,
        RuleField::Artist => 1,
        RuleField::AlbumArtist => 2,
        RuleField::Album => 3,
        RuleField::Genre => 4,
        RuleField::Year => 5,
        RuleField::TrackNumber => 6,
        RuleField::DiscNumber => 7,
        RuleField::Duration => 8,
        RuleField::Bitrate => 9,
        RuleField::SampleRate => 10,
        RuleField::Format => 11,
        RuleField::PlayCount => 12,
        RuleField::DateAdded => 13,
        RuleField::DateModified => 14,
        RuleField::FileSize => 15,
    }
}

/// Determine the field type category for operator selection.
enum FieldType {
    Text,
    Number,
    Date,
}

fn field_type(field: &RuleField) -> FieldType {
    match field {
        RuleField::Title
        | RuleField::Artist
        | RuleField::AlbumArtist
        | RuleField::Album
        | RuleField::Genre
        | RuleField::Format => FieldType::Text,
        RuleField::DateAdded | RuleField::DateModified => FieldType::Date,
        _ => FieldType::Number,
    }
}

/// Text operator names.
const TEXT_OPS: &[&str] = &[
    "is",
    "is not",
    "contains",
    "does not contain",
    "starts with",
    "ends with",
];

/// Numeric operator names.
const NUM_OPS: &[&str] = &["is", "is not", "greater than", "less than", "in range"];

/// Date operator names.
const DATE_OPS: &[&str] = &[
    "is",
    "is not",
    "is before",
    "is after",
    "is in the last",
    "is not in the last",
];

/// Limit unit names.
const LIMIT_UNITS: &[&str] = &["items", "minutes", "hours", "MB", "GB"];

/// Limit sort-by names.
const LIMIT_SORTS: &[&str] = &[
    "Random",
    "Title",
    "Album",
    "Artist",
    "Genre",
    "Year",
    "Bitrate",
    "Most Played",
    "Least Played",
    "Most Recently Added",
    "Least Recently Added",
    "Most Recently Played",
    "Least Recently Played",
];

// ── Public API ──────────────────────────────────────────────────────

/// Show the smart playlist editor dialog.
///
/// `existing_rules` is `Some` when editing an existing smart playlist,
/// `None` when creating a new one.
///
/// `on_save` is called with the final `SmartRules` when the user clicks OK.
pub fn show_smart_playlist_editor(
    parent: &impl IsA<gtk::Widget>,
    playlist_name: &str,
    existing_rules: Option<&SmartRules>,
    on_save: impl Fn(SmartRules) + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(if existing_rules.is_some() {
            format!("Edit Smart Playlist: {playlist_name}")
        } else {
            "New Smart Playlist".to_string()
        })
        .close_response("cancel")
        .default_response("ok")
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    // ── Match mode ──────────────────────────────────────────────────
    let match_model = gtk::StringList::new(&["All", "Any"]);
    let match_dropdown = gtk::DropDown::builder()
        .model(&match_model)
        .selected(match existing_rules {
            Some(r) if r.match_mode == MatchMode::Any => 1,
            _ => 0,
        })
        .build();

    let match_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    match_row.append(&gtk::Label::new(Some("Match")));
    match_row.append(&match_dropdown);
    match_row.append(&gtk::Label::new(Some("of the following rules:")));

    // ── Rules list ──────────────────────────────────────────────────
    let rules_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();

    let rules_box_rc = std::rc::Rc::new(std::cell::RefCell::new(rules_box.clone()));

    // Populate with existing rules or one empty rule.
    let initial_rules = existing_rules.map(|r| r.rules.clone()).unwrap_or_else(|| {
        vec![SmartRule {
            field: RuleField::Genre,
            operator: RuleOperator::Contains,
            value: RuleValue::Text(String::new()),
        }]
    });

    for rule in &initial_rules {
        let row = build_rule_row(Some(rule), rules_box_rc.clone());
        rules_box.append(&row);
    }

    // ── Add rule button ─────────────────────────────────────────────
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Add rule")
        .build();
    {
        let rb = rules_box_rc.clone();
        add_btn.connect_clicked(move |_| {
            let row = build_rule_row(None, rb.clone());
            rb.borrow().append(&row);
        });
    }

    // ── Limit section ───────────────────────────────────────────────
    let limit_check = gtk::CheckButton::builder()
        .label("Limit to")
        .active(existing_rules.is_some_and(|r| r.limit.is_some()))
        .build();

    let limit_value = gtk::SpinButton::with_range(1.0, 99999.0, 1.0);
    limit_value.set_value(
        existing_rules
            .and_then(|r| r.limit.as_ref())
            .map(|l| l.value as f64)
            .unwrap_or(25.0),
    );

    let limit_unit_model = gtk::StringList::new(LIMIT_UNITS);
    let limit_unit_dropdown = gtk::DropDown::builder()
        .model(&limit_unit_model)
        .selected(
            existing_rules
                .and_then(|r| r.limit.as_ref())
                .map(|l| match l.unit {
                    LimitUnit::Items => 0,
                    LimitUnit::Minutes => 1,
                    LimitUnit::Hours => 2,
                    LimitUnit::MB => 3,
                    LimitUnit::GB => 4,
                })
                .unwrap_or(0),
        )
        .build();

    let limit_sort_model = gtk::StringList::new(LIMIT_SORTS);
    let limit_sort_dropdown = gtk::DropDown::builder()
        .model(&limit_sort_model)
        .selected(
            existing_rules
                .and_then(|r| r.limit.as_ref())
                .map(|l| match l.selected_by {
                    LimitSort::Random => 0,
                    LimitSort::Title => 1,
                    LimitSort::Album => 2,
                    LimitSort::Artist => 3,
                    LimitSort::Genre => 4,
                    LimitSort::Year => 5,
                    LimitSort::Bitrate => 6,
                    LimitSort::MostPlayed => 7,
                    LimitSort::LeastPlayed => 8,
                    LimitSort::MostRecentlyAdded => 9,
                    LimitSort::LeastRecentlyAdded => 10,
                    LimitSort::MostRecentlyPlayed => 11,
                    LimitSort::LeastRecentlyPlayed => 12,
                })
                .unwrap_or(0),
        )
        .build();

    let limit_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    limit_row.append(&limit_check);
    limit_row.append(&limit_value);
    limit_row.append(&limit_unit_dropdown);
    limit_row.append(&gtk::Label::new(Some("selected by")));
    limit_row.append(&limit_sort_dropdown);

    // ── Live updating ───────────────────────────────────────────────
    let live_check = gtk::CheckButton::builder()
        .label("Live updating")
        .active(existing_rules.map(|r| r.live_updating).unwrap_or(true))
        .build();

    // ── Layout ──────────────────────────────────────────────────────
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(8)
        .build();
    vbox.append(&match_row);

    let rules_scroll = gtk::ScrolledWindow::builder()
        .child(&rules_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(120)
        .max_content_height(300)
        .build();
    vbox.append(&rules_scroll);

    let add_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::End)
        .build();
    add_row.append(&add_btn);
    vbox.append(&add_row);
    vbox.append(&limit_row);
    vbox.append(&live_check);

    // ── Sort order section ──────────────────────────────────────────
    let sort_label = gtk::Label::builder()
        .label("Sort by:")
        .halign(gtk::Align::Start)
        .margin_top(4)
        .build();
    vbox.append(&sort_label);

    let sort_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();

    let sort_box_rc = std::rc::Rc::new(std::cell::RefCell::new(sort_box.clone()));

    // Populate with existing sort criteria.
    let initial_sort = existing_rules
        .map(|r| r.sort_order.clone())
        .unwrap_or_default();
    for criterion in &initial_sort {
        let row = build_sort_row(Some(criterion), sort_box_rc.clone());
        sort_box.append(&row);
    }

    let add_sort_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Add sort level")
        .halign(gtk::Align::End)
        .build();
    {
        let sb = sort_box_rc.clone();
        add_sort_btn.connect_clicked(move |_| {
            let row = build_sort_row(None, sb.clone());
            sb.borrow().append(&row);
        });
    }

    vbox.append(&sort_box);
    vbox.append(&add_sort_btn);

    dialog.set_extra_child(Some(&vbox));

    // ── Response handler ────────────────────────────────────────────
    let rules_box_for_save = rules_box_rc.clone();
    let sort_box_for_save = sort_box_rc.clone();

    dialog.connect_response(None, move |_dialog, response| {
        if response != "ok" {
            return;
        }

        // Collect rules from the UI.
        let rules_box = rules_box_for_save.borrow();
        let mut rules = Vec::new();

        let mut child = rules_box.first_child();
        while let Some(widget) = child {
            if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                if let Some(rule) = extract_rule_from_row(row) {
                    rules.push(rule);
                }
            }
            child = widget.next_sibling();
        }

        // Collect sort criteria from the UI.
        let sort_box = sort_box_for_save.borrow();
        let mut sort_order = Vec::new();
        let mut child = sort_box.first_child();
        while let Some(widget) = child {
            if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                if let Some(criterion) = extract_sort_from_row(row) {
                    sort_order.push(criterion);
                }
            }
            child = widget.next_sibling();
        }

        let match_mode = if match_dropdown.selected() == 1 {
            MatchMode::Any
        } else {
            MatchMode::All
        };

        let limit = if limit_check.is_active() {
            let unit = match limit_unit_dropdown.selected() {
                1 => LimitUnit::Minutes,
                2 => LimitUnit::Hours,
                3 => LimitUnit::MB,
                4 => LimitUnit::GB,
                _ => LimitUnit::Items,
            };
            let selected_by = match limit_sort_dropdown.selected() {
                1 => LimitSort::Title,
                2 => LimitSort::Album,
                3 => LimitSort::Artist,
                4 => LimitSort::Genre,
                5 => LimitSort::Year,
                6 => LimitSort::Bitrate,
                7 => LimitSort::MostPlayed,
                8 => LimitSort::LeastPlayed,
                9 => LimitSort::MostRecentlyAdded,
                10 => LimitSort::LeastRecentlyAdded,
                11 => LimitSort::MostRecentlyPlayed,
                12 => LimitSort::LeastRecentlyPlayed,
                _ => LimitSort::Random,
            };
            Some(SmartLimit {
                value: limit_value.value() as u32,
                unit,
                selected_by,
            })
        } else {
            None
        };

        let smart_rules = SmartRules {
            match_mode,
            rules,
            limit,
            live_updating: live_check.is_active(),
            sort_order,
        };

        on_save(smart_rules);
    });

    dialog.present(Some(parent));
}

// ── Rule row builder ────────────────────────────────────────────────

/// Build a single rule row with field, operator, and value widgets.
fn build_rule_row(
    existing: Option<&SmartRule>,
    rules_box: std::rc::Rc<std::cell::RefCell<gtk::Box>>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();

    // Field dropdown.
    let field_model = gtk::StringList::new(FIELD_NAMES);
    let field_dropdown = gtk::DropDown::builder()
        .model(&field_model)
        .selected(existing.map(|r| field_to_index(&r.field)).unwrap_or(0))
        .build();

    // Operator dropdown (populated dynamically).
    let op_model = gtk::StringList::new(&[] as &[&str]);
    let op_dropdown = gtk::DropDown::builder()
        .model(&op_model)
        .selected(0)
        .build();

    // Value entry.
    let value_entry = gtk::Entry::builder()
        .placeholder_text("value")
        .hexpand(true)
        .width_chars(12)
        .build();

    // Second value entry (for "in range").
    let value2_entry = gtk::Entry::builder()
        .placeholder_text("to")
        .width_chars(8)
        .visible(false)
        .build();

    // Remove button.
    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Remove rule")
        .build();

    row.append(&field_dropdown);
    row.append(&op_dropdown);
    row.append(&value_entry);
    row.append(&value2_entry);
    row.append(&remove_btn);

    // Wire remove button.
    {
        let rb = rules_box.clone();
        let row_ref = row.clone();
        remove_btn.connect_clicked(move |_| {
            rb.borrow().remove(&row_ref);
        });
    }

    // Wire field dropdown to update operators.
    {
        let op_model = op_model.clone();
        let op_dropdown = op_dropdown.clone();
        let value2 = value2_entry.clone();

        let update_ops = move |field_idx: u32| {
            let field = index_to_field(field_idx);
            let ops: &[&str] = match field_type(&field) {
                FieldType::Text => TEXT_OPS,
                FieldType::Number => NUM_OPS,
                FieldType::Date => DATE_OPS,
            };

            // Clear and repopulate.
            while op_model.n_items() > 0 {
                op_model.remove(0);
            }
            for op in ops {
                op_model.append(op);
            }
            op_dropdown.set_selected(0);
            value2.set_visible(false);
        };

        // Initial population.
        update_ops(field_dropdown.selected());

        field_dropdown.connect_selected_notify(move |dd| {
            update_ops(dd.selected());
        });
    }

    // Wire operator dropdown to show/hide range field.
    {
        let value2 = value2_entry.clone();
        let field_dd = field_dropdown.clone();
        op_dropdown.connect_selected_notify(move |dd| {
            let field = index_to_field(field_dd.selected());
            let is_range = match field_type(&field) {
                FieldType::Number => dd.selected() == 4, // "in range"
                _ => false,
            };
            value2.set_visible(is_range);
        });
    }

    // Pre-populate from existing rule.
    if let Some(rule) = existing {
        // Set operator index.
        let op_idx = match field_type(&rule.field) {
            FieldType::Text => match &rule.operator {
                RuleOperator::Is => 0,
                RuleOperator::IsNot => 1,
                RuleOperator::Contains => 2,
                RuleOperator::DoesNotContain => 3,
                RuleOperator::StartsWith => 4,
                RuleOperator::EndsWith => 5,
                _ => 0,
            },
            FieldType::Number => match &rule.operator {
                RuleOperator::Is => 0,
                RuleOperator::IsNot => 1,
                RuleOperator::GreaterThan => 2,
                RuleOperator::LessThan => 3,
                RuleOperator::InRange => 4,
                _ => 0,
            },
            FieldType::Date => match &rule.operator {
                RuleOperator::Is => 0,
                RuleOperator::IsNot => 1,
                RuleOperator::IsBefore => 2,
                RuleOperator::IsAfter => 3,
                RuleOperator::IsInTheLast { .. } => 4,
                RuleOperator::IsNotInTheLast { .. } => 5,
                _ => 0,
            },
        };
        op_dropdown.set_selected(op_idx);

        // Set value.
        match &rule.value {
            RuleValue::Text(s) => value_entry.set_text(s),
            RuleValue::Number(n) => value_entry.set_text(&n.to_string()),
            RuleValue::NumberRange(lo, hi) => {
                value_entry.set_text(&lo.to_string());
                value2_entry.set_text(&hi.to_string());
                value2_entry.set_visible(true);
            }
            RuleValue::Date(d) => value_entry.set_text(d),
            RuleValue::Duration(d) => value_entry.set_text(&d.to_string()),
            RuleValue::Size(s) => value_entry.set_text(&s.to_string()),
        }
    }

    // Store widget names for extraction.
    field_dropdown.set_widget_name("field");
    op_dropdown.set_widget_name("operator");
    value_entry.set_widget_name("value");
    value2_entry.set_widget_name("value2");

    row
}

/// Extract a `SmartRule` from a rule row's widgets.
fn extract_rule_from_row(row: &gtk::Box) -> Option<SmartRule> {
    let mut field_dropdown: Option<gtk::DropDown> = None;
    let mut op_dropdown: Option<gtk::DropDown> = None;
    let mut value_entry: Option<gtk::Entry> = None;
    let mut value2_entry: Option<gtk::Entry> = None;

    let mut child = row.first_child();
    while let Some(widget) = child {
        let name = widget.widget_name();
        if name == "field" {
            field_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned();
        } else if name == "operator" {
            op_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned();
        } else if name == "value" {
            value_entry = widget.downcast_ref::<gtk::Entry>().cloned();
        } else if name == "value2" {
            value2_entry = widget.downcast_ref::<gtk::Entry>().cloned();
        }
        child = widget.next_sibling();
    }

    let field_dd = field_dropdown?;
    let op_dd = op_dropdown?;
    let val_entry = value_entry?;

    let field = index_to_field(field_dd.selected());
    let val_text = val_entry.text().to_string();
    let val2_text = value2_entry
        .map(|e| e.text().to_string())
        .unwrap_or_default();

    let (operator, value) = match field_type(&field) {
        FieldType::Text => {
            let op = match op_dd.selected() {
                0 => RuleOperator::Is,
                1 => RuleOperator::IsNot,
                2 => RuleOperator::Contains,
                3 => RuleOperator::DoesNotContain,
                4 => RuleOperator::StartsWith,
                5 => RuleOperator::EndsWith,
                _ => RuleOperator::Contains,
            };
            (op, RuleValue::Text(val_text))
        }
        FieldType::Number => {
            let op = match op_dd.selected() {
                0 => RuleOperator::Is,
                1 => RuleOperator::IsNot,
                2 => RuleOperator::GreaterThan,
                3 => RuleOperator::LessThan,
                4 => RuleOperator::InRange,
                _ => RuleOperator::Is,
            };
            if matches!(op, RuleOperator::InRange) {
                let lo = val_text.parse::<i64>().unwrap_or(0);
                let hi = val2_text.parse::<i64>().unwrap_or(0);
                (op, RuleValue::NumberRange(lo, hi))
            } else {
                let n = val_text.parse::<i64>().unwrap_or(0);
                (op, RuleValue::Number(n))
            }
        }
        FieldType::Date => {
            let op = match op_dd.selected() {
                0 => RuleOperator::Is,
                1 => RuleOperator::IsNot,
                2 => RuleOperator::IsBefore,
                3 => RuleOperator::IsAfter,
                4 => {
                    let amount = val_text.parse::<u32>().unwrap_or(30);
                    RuleOperator::IsInTheLast {
                        amount,
                        unit: DateUnit::Days,
                    }
                }
                5 => {
                    let amount = val_text.parse::<u32>().unwrap_or(30);
                    RuleOperator::IsNotInTheLast {
                        amount,
                        unit: DateUnit::Days,
                    }
                }
                _ => RuleOperator::Is,
            };
            match &op {
                RuleOperator::IsInTheLast { .. } | RuleOperator::IsNotInTheLast { .. } => {
                    // Value is the amount (already embedded in the operator).
                    (op, RuleValue::Number(val_text.parse::<i64>().unwrap_or(30)))
                }
                _ => (op, RuleValue::Date(val_text)),
            }
        }
    };

    Some(SmartRule {
        field,
        operator,
        value,
    })
}

// ── Sort row builder ────────────────────────────────────────────────

/// Sort field names for the dropdown (must match `SortField` enum order).
const SORT_FIELD_NAMES: &[&str] = &[
    "Artist",
    "Album Artist",
    "Album",
    "Title",
    "Year",
    "Track Number",
    "Disc Number",
    "Genre",
    "Duration",
    "Bitrate",
    "Play Count",
    "Date Added",
    "Date Modified",
];

/// Map dropdown index to `SortField`.
fn index_to_sort_field(idx: u32) -> SortField {
    match idx {
        0 => SortField::Artist,
        1 => SortField::AlbumArtist,
        2 => SortField::Album,
        3 => SortField::Title,
        4 => SortField::Year,
        5 => SortField::TrackNumber,
        6 => SortField::DiscNumber,
        7 => SortField::Genre,
        8 => SortField::Duration,
        9 => SortField::Bitrate,
        10 => SortField::PlayCount,
        11 => SortField::DateAdded,
        12 => SortField::DateModified,
        _ => SortField::Artist,
    }
}

/// Map `SortField` to dropdown index.
fn sort_field_to_index(field: SortField) -> u32 {
    match field {
        SortField::Artist => 0,
        SortField::AlbumArtist => 1,
        SortField::Album => 2,
        SortField::Title => 3,
        SortField::Year => 4,
        SortField::TrackNumber => 5,
        SortField::DiscNumber => 6,
        SortField::Genre => 7,
        SortField::Duration => 8,
        SortField::Bitrate => 9,
        SortField::PlayCount => 10,
        SortField::DateAdded => 11,
        SortField::DateModified => 12,
    }
}

/// Build a single sort criterion row with field dropdown and direction toggle.
fn build_sort_row(
    existing: Option<&SortCriterion>,
    sort_box: std::rc::Rc<std::cell::RefCell<gtk::Box>>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();

    let field_model = gtk::StringList::new(SORT_FIELD_NAMES);
    let field_dropdown = gtk::DropDown::builder()
        .model(&field_model)
        .selected(existing.map(|c| sort_field_to_index(c.field)).unwrap_or(0))
        .hexpand(true)
        .build();

    let dir_model = gtk::StringList::new(&["Ascending", "Descending"]);
    let dir_dropdown = gtk::DropDown::builder()
        .model(&dir_model)
        .selected(
            existing
                .map(|c| u32::from(c.direction == SortDirection::Descending))
                .unwrap_or(0),
        )
        .build();

    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Remove sort level")
        .build();

    row.append(&field_dropdown);
    row.append(&dir_dropdown);
    row.append(&remove_btn);

    // Wire remove button.
    {
        let sb = sort_box;
        let row_ref = row.clone();
        remove_btn.connect_clicked(move |_| {
            sb.borrow().remove(&row_ref);
        });
    }

    // Store widget names for extraction.
    field_dropdown.set_widget_name("sort-field");
    dir_dropdown.set_widget_name("sort-dir");

    row
}

/// Extract a `SortCriterion` from a sort row's widgets.
fn extract_sort_from_row(row: &gtk::Box) -> Option<SortCriterion> {
    let mut field_dropdown: Option<gtk::DropDown> = None;
    let mut dir_dropdown: Option<gtk::DropDown> = None;

    let mut child = row.first_child();
    while let Some(widget) = child {
        let name = widget.widget_name();
        if name == "sort-field" {
            field_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned();
        } else if name == "sort-dir" {
            dir_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned();
        }
        child = widget.next_sibling();
    }

    let field_dd = field_dropdown?;
    let dir_dd = dir_dropdown?;

    let field = index_to_sort_field(field_dd.selected());
    let direction = if dir_dd.selected() == 1 {
        SortDirection::Descending
    } else {
        SortDirection::Ascending
    };

    Some(SortCriterion { field, direction })
}
