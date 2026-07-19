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
    "Composer",
    "Year",
    "Track Number",
    "Disc Number",
    "Duration (sec)",
    "Bitrate (kbps)",
    "Sample Rate (Hz)",
    "Format",
    "Play Count",
    "Last Played",
    "Date Added",
    "Date Modified",
    "File Size (bytes)",
    "Rating (1–100)",
];

/// Map dropdown index to `RuleField`.
fn index_to_field(idx: u32) -> RuleField {
    match idx {
        0 => RuleField::Title,
        1 => RuleField::Artist,
        2 => RuleField::AlbumArtist,
        3 => RuleField::Album,
        4 => RuleField::Genre,
        5 => RuleField::Composer,
        6 => RuleField::Year,
        7 => RuleField::TrackNumber,
        8 => RuleField::DiscNumber,
        9 => RuleField::Duration,
        10 => RuleField::Bitrate,
        11 => RuleField::SampleRate,
        12 => RuleField::Format,
        13 => RuleField::PlayCount,
        14 => RuleField::LastPlayed,
        15 => RuleField::DateAdded,
        16 => RuleField::DateModified,
        17 => RuleField::FileSize,
        18 => RuleField::Rating,
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
        RuleField::Composer => 5,
        RuleField::Year => 6,
        RuleField::TrackNumber => 7,
        RuleField::DiscNumber => 8,
        RuleField::Duration => 9,
        RuleField::Bitrate => 10,
        RuleField::SampleRate => 11,
        RuleField::Format => 12,
        RuleField::PlayCount => 13,
        RuleField::LastPlayed => 14,
        RuleField::DateAdded => 15,
        RuleField::DateModified => 16,
        RuleField::FileSize => 17,
        RuleField::Rating => 18,
    }
}

/// Determine the field type category for operator selection.
#[derive(Clone, Copy)]
enum FieldType {
    Text,
    Number,
    Date,
    Rating,
}

fn field_type(field: &RuleField) -> FieldType {
    match field {
        RuleField::Title
        | RuleField::Artist
        | RuleField::AlbumArtist
        | RuleField::Album
        | RuleField::Genre
        | RuleField::Composer
        | RuleField::Format => FieldType::Text,
        RuleField::LastPlayed | RuleField::DateAdded | RuleField::DateModified => FieldType::Date,
        RuleField::Rating => FieldType::Rating,
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

/// Rating operators retain the numeric operator indexes and append explicit
/// presence predicates. This keeps existing editor mappings stable.
const RATING_OPS: &[&str] = &[
    "is",
    "is not",
    "greater than",
    "less than",
    "in range",
    "is rated",
    "is unrated",
];

fn index_to_rating_operator(idx: u32) -> RuleOperator {
    match idx {
        0 => RuleOperator::Is,
        1 => RuleOperator::IsNot,
        2 => RuleOperator::GreaterThan,
        3 => RuleOperator::LessThan,
        4 => RuleOperator::InRange,
        5 => RuleOperator::IsRated,
        6 => RuleOperator::IsUnrated,
        _ => RuleOperator::Is,
    }
}

fn rating_operator_to_index(operator: &RuleOperator) -> u32 {
    match operator {
        RuleOperator::Is => 0,
        RuleOperator::IsNot => 1,
        RuleOperator::GreaterThan => 2,
        RuleOperator::LessThan => 3,
        RuleOperator::InRange => 4,
        RuleOperator::IsRated => 5,
        RuleOperator::IsUnrated => 6,
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatingRuleInputError {
    NotAnInteger,
    OutOfRange,
    ReversedRange,
}

impl RatingRuleInputError {
    fn message(self, locale: &str) -> String {
        match self {
            Self::NotAnInteger => {
                rust_i18n::t!("ratings.rule_not_integer", locale = locale).into_owned()
            }
            Self::OutOfRange => {
                rust_i18n::t!("ratings.rule_out_of_range", locale = locale).into_owned()
            }
            Self::ReversedRange => {
                rust_i18n::t!("ratings.rule_reversed_range", locale = locale).into_owned()
            }
        }
    }
}

fn canonical_rating_operand(raw: &str) -> Result<i64, RatingRuleInputError> {
    let value = raw
        .trim()
        .parse::<i64>()
        .map_err(|_| RatingRuleInputError::NotAnInteger)?;
    if !(1..=100).contains(&value) {
        return Err(RatingRuleInputError::OutOfRange);
    }
    Ok(value)
}

fn rating_rule_from_editor(
    op_index: u32,
    raw_value: &str,
    raw_high: &str,
) -> Result<SmartRule, RatingRuleInputError> {
    let operator = index_to_rating_operator(op_index);
    let value = match operator {
        RuleOperator::InRange => {
            let low = canonical_rating_operand(raw_value)?;
            let high = canonical_rating_operand(raw_high)?;
            if low > high {
                return Err(RatingRuleInputError::ReversedRange);
            }
            RuleValue::NumberRange(low, high)
        }
        RuleOperator::IsRated | RuleOperator::IsUnrated => {
            // SmartRule retains its historical required value field. Presence
            // operators validate this canonical placeholder before otherwise
            // ignoring it during evaluation.
            RuleValue::Number(1)
        }
        _ => RuleValue::Number(canonical_rating_operand(raw_value)?),
    };

    Ok(SmartRule {
        field: RuleField::Rating,
        operator,
        value,
    })
}

fn set_rating_entry_error(entry: &gtk::Entry, message: Option<&str>) {
    if let Some(message) = message {
        entry.add_css_class("error");
        entry.update_property(&[gtk::accessible::Property::Description(message)]);
    } else {
        entry.remove_css_class("error");
        entry.reset_property(gtk::AccessibleProperty::Description);
    }
}

/// Validate one rendered row without changing either operand.
///
/// The visible error label and each invalid entry's accessible description
/// carry the same message. Presence predicates require no user input because
/// the editor supplies their canonical inert placeholder itself.
fn validate_rating_row(row: &gtk::Box) -> bool {
    let mut field_dropdown: Option<gtk::DropDown> = None;
    let mut op_dropdown: Option<gtk::DropDown> = None;
    let mut value_entry: Option<gtk::Entry> = None;
    let mut value2_entry: Option<gtk::Entry> = None;
    let mut error_label: Option<gtk::Label> = None;

    let mut child = row.first_child();
    while let Some(widget) = child {
        match widget.widget_name().as_str() {
            "field" => field_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned(),
            "operator" => op_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned(),
            "value" => value_entry = widget.downcast_ref::<gtk::Entry>().cloned(),
            "value2" => value2_entry = widget.downcast_ref::<gtk::Entry>().cloned(),
            "rating_error" => error_label = widget.downcast_ref::<gtk::Label>().cloned(),
            _ => {}
        }
        child = widget.next_sibling();
    }

    let (Some(field), Some(operator), Some(value), Some(high), Some(error)) = (
        field_dropdown,
        op_dropdown,
        value_entry,
        value2_entry,
        error_label,
    ) else {
        return false;
    };

    if index_to_field(field.selected()) != RuleField::Rating {
        error.set_visible(false);
        set_rating_entry_error(&value, None);
        set_rating_entry_error(&high, None);
        return true;
    }

    let validation = rating_rule_from_editor(operator.selected(), &value.text(), &high.text());
    let locale = rust_i18n::locale();
    let message = validation
        .as_ref()
        .err()
        .copied()
        .map(|error| error.message(locale.as_ref()));
    error.set_label(message.as_deref().unwrap_or_default());
    error.set_visible(message.is_some());

    let is_range = operator.selected() == 4;
    set_rating_entry_error(&value, message.as_deref());
    set_rating_entry_error(&high, if is_range { message.as_deref() } else { None });
    validation.is_ok()
}

fn refresh_rating_validation(dialog: &adw::AlertDialog, rules_box: &gtk::Box) -> bool {
    let mut valid = true;
    let mut child = rules_box.first_child();
    while let Some(widget) = child {
        if let Some(row) = widget.downcast_ref::<gtk::Box>() {
            valid &= validate_rating_row(row);
        }
        child = widget.next_sibling();
    }
    dialog.set_response_enabled("ok", valid);
    valid
}

/// Date operator names.
const DATE_OPS: &[&str] = &[
    "is",
    "is not",
    "is before",
    "is after",
    "is in the last",
    "is not in the last",
];

/// Relative-date unit names.
const DATE_UNITS: &[&str] = &["days", "weeks", "months"];

fn index_to_date_unit(idx: u32) -> DateUnit {
    match idx {
        1 => DateUnit::Weeks,
        2 => DateUnit::Months,
        _ => DateUnit::Days,
    }
}

fn date_unit_to_index(unit: DateUnit) -> u32 {
    match unit {
        DateUnit::Days => 0,
        DateUnit::Weeks => 1,
        DateUnit::Months => 2,
    }
}

fn relative_date_unit(operator: &RuleOperator) -> Option<DateUnit> {
    match operator {
        RuleOperator::IsInTheLast { unit, .. } | RuleOperator::IsNotInTheLast { unit, .. } => {
            Some(*unit)
        }
        _ => None,
    }
}

fn relative_date_operator(op_index: u32, amount: u32, unit: DateUnit) -> Option<RuleOperator> {
    match op_index {
        4 => Some(RuleOperator::IsInTheLast { amount, unit }),
        5 => Some(RuleOperator::IsNotInTheLast { amount, unit }),
        _ => None,
    }
}

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
    "Highest Rated",
    "Lowest Rated",
];

fn index_to_limit_sort(idx: u32) -> LimitSort {
    match idx {
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
        13 => LimitSort::HighestRated,
        14 => LimitSort::LowestRated,
        _ => LimitSort::Random,
    }
}

fn limit_sort_to_index(sort: LimitSort) -> u32 {
    match sort {
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
        LimitSort::HighestRated => 13,
        LimitSort::LowestRated => 14,
    }
}

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

    let rules_box_weak = rules_box.downgrade();
    let dialog_weak = dialog.downgrade();

    // Populate with existing rules or one empty rule.
    let initial_rules = existing_rules.map(|r| r.rules.clone()).unwrap_or_else(|| {
        vec![SmartRule {
            field: RuleField::Genre,
            operator: RuleOperator::Contains,
            value: RuleValue::Text(String::new()),
        }]
    });

    for rule in &initial_rules {
        let row = build_rule_row(Some(rule), rules_box_weak.clone(), dialog_weak.clone());
        rules_box.append(&row);
    }
    refresh_rating_validation(&dialog, &rules_box);

    // ── Add rule button ─────────────────────────────────────────────
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Add rule")
        .build();
    {
        let rules_box = rules_box_weak.clone();
        let dialog = dialog_weak.clone();
        add_btn.connect_clicked(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            let row = build_rule_row(None, rules_box.downgrade(), dialog.downgrade());
            rules_box.append(&row);
            refresh_rating_validation(&dialog, &rules_box);
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
                .map(|l| limit_sort_to_index(l.selected_by))
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

    let sort_box_weak = sort_box.downgrade();

    // Populate with existing sort criteria.
    let initial_sort = existing_rules
        .map(|r| r.sort_order.clone())
        .unwrap_or_default();
    for criterion in &initial_sort {
        let row = build_sort_row(Some(criterion), sort_box_weak.clone());
        sort_box.append(&row);
    }

    let add_sort_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Add sort level")
        .halign(gtk::Align::End)
        .build();
    {
        let sort_box = sort_box_weak.clone();
        add_sort_btn.connect_clicked(move |_| {
            let Some(sort_box) = sort_box.upgrade() else {
                return;
            };
            let row = build_sort_row(None, sort_box.downgrade());
            sort_box.append(&row);
        });
    }

    vbox.append(&sort_box);
    vbox.append(&add_sort_btn);

    dialog.set_extra_child(Some(&vbox));

    // ── Response handler ────────────────────────────────────────────
    let rules_box_for_save = rules_box_weak;
    let sort_box_for_save = sort_box_weak;

    dialog.connect_response(None, move |dialog, response| {
        if response != "ok" {
            return;
        }
        let Some(rules_box) = rules_box_for_save.upgrade() else {
            return;
        };
        if !refresh_rating_validation(dialog, &rules_box) {
            return;
        }

        // Collect rules from the UI.
        let mut rules = Vec::new();

        let mut child = rules_box.first_child();
        while let Some(widget) = child {
            if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                let Some(rule) = extract_rule_from_row(row) else {
                    return;
                };
                rules.push(rule);
            }
            child = widget.next_sibling();
        }

        // Collect sort criteria from the UI.
        let Some(sort_box) = sort_box_for_save.upgrade() else {
            return;
        };
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
            let selected_by = index_to_limit_sort(limit_sort_dropdown.selected());
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
            sort_order,
        };

        on_save(smart_rules);
    });

    dialog.present(Some(parent));
}

// ── Rule row builder ────────────────────────────────────────────────

fn update_rule_operator_widgets(
    field_idx: u32,
    op_model: &gtk::StringList,
    op_dropdown: &gtk::DropDown,
    value: &gtk::Entry,
    value2: &gtk::Entry,
    date_unit: &gtk::DropDown,
) {
    let field = index_to_field(field_idx);
    let ops: &[&str] = match field_type(&field) {
        FieldType::Text => TEXT_OPS,
        FieldType::Number => NUM_OPS,
        FieldType::Date => DATE_OPS,
        FieldType::Rating => RATING_OPS,
    };

    while op_model.n_items() > 0 {
        op_model.remove(0);
    }
    for op in ops {
        op_model.append(op);
    }
    op_dropdown.set_selected(0);
    value.set_visible(true);
    value.set_placeholder_text(if matches!(field_type(&field), FieldType::Rating) {
        Some("1–100")
    } else {
        Some("value")
    });
    value2.set_visible(false);
    date_unit.set_visible(false);
}

/// Build a single rule row with field, operator, and value widgets.
fn build_rule_row(
    existing: Option<&SmartRule>,
    rules_box: gtk::glib::WeakRef<gtk::Box>,
    dialog: gtk::glib::WeakRef<adw::AlertDialog>,
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

    // Unit selector for relative date operators. It remains part of every
    // row so switching fields/operators cannot lose a previously selected
    // Weeks/Months value, but is visible only for the two relative modes.
    let date_unit_model = gtk::StringList::new(DATE_UNITS);
    let date_unit_dropdown = gtk::DropDown::builder()
        .model(&date_unit_model)
        .selected(
            existing
                .and_then(|rule| relative_date_unit(&rule.operator))
                .map(date_unit_to_index)
                .unwrap_or(0),
        )
        .visible(false)
        .build();

    let rating_error = gtk::Label::builder()
        .css_classes(["error"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .visible(false)
        .build();
    rating_error.set_accessible_role(gtk::AccessibleRole::Alert);

    // Remove button.
    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Remove rule")
        .build();

    row.append(&field_dropdown);
    row.append(&op_dropdown);
    row.append(&value_entry);
    row.append(&date_unit_dropdown);
    row.append(&value2_entry);
    row.append(&rating_error);
    row.append(&remove_btn);

    // Wire remove button.
    {
        let rules_box = rules_box.clone();
        let row = row.downgrade();
        let dialog = dialog.clone();
        remove_btn.connect_clicked(move |_| {
            let (Some(rules_box), Some(row)) = (rules_box.upgrade(), row.upgrade()) else {
                return;
            };
            rules_box.remove(&row);
            if let Some(dialog) = dialog.upgrade() {
                refresh_rating_validation(&dialog, &rules_box);
            }
        });
    }

    // Wire field dropdown to update operators.
    {
        // Initial population.
        update_rule_operator_widgets(
            field_dropdown.selected(),
            &op_model,
            &op_dropdown,
            &value_entry,
            &value2_entry,
            &date_unit_dropdown,
        );

        let op_model = op_model.downgrade();
        let op_dropdown = op_dropdown.downgrade();
        let value = value_entry.downgrade();
        let value2 = value2_entry.downgrade();
        let date_unit = date_unit_dropdown.downgrade();
        field_dropdown.connect_selected_notify(move |dd| {
            let (Some(op_model), Some(op_dropdown), Some(value), Some(value2), Some(date_unit)) = (
                op_model.upgrade(),
                op_dropdown.upgrade(),
                value.upgrade(),
                value2.upgrade(),
                date_unit.upgrade(),
            ) else {
                return;
            };
            update_rule_operator_widgets(
                dd.selected(),
                &op_model,
                &op_dropdown,
                &value,
                &value2,
                &date_unit,
            );
        });
    }

    // Wire operator dropdown to show/hide range field.
    {
        let value2 = value2_entry.downgrade();
        let value = value_entry.downgrade();
        let field_dd = field_dropdown.downgrade();
        let date_unit = date_unit_dropdown.downgrade();
        op_dropdown.connect_selected_notify(move |dd| {
            let (Some(value2), Some(value), Some(field_dd), Some(date_unit)) = (
                value2.upgrade(),
                value.upgrade(),
                field_dd.upgrade(),
                date_unit.upgrade(),
            ) else {
                return;
            };
            let field = index_to_field(field_dd.selected());
            let field_type = field_type(&field);
            let is_range = match field_type {
                FieldType::Number | FieldType::Rating => dd.selected() == 4, // "in range"
                _ => false,
            };
            value2.set_visible(is_range);
            let is_rating_presence =
                matches!(field_type, FieldType::Rating) && matches!(dd.selected(), 5 | 6);
            value.set_visible(!is_rating_presence);
            let is_relative_date =
                matches!(field_type, FieldType::Date) && matches!(dd.selected(), 4 | 5);
            date_unit.set_visible(is_relative_date);
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
            FieldType::Rating => rating_operator_to_index(&rule.operator),
        };
        op_dropdown.set_selected(op_idx);
        date_unit_dropdown.set_visible(relative_date_unit(&rule.operator).is_some());

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
        if let RuleOperator::IsInTheLast { amount, .. }
        | RuleOperator::IsNotInTheLast { amount, .. } = &rule.operator
        {
            // The amount embedded in the operator is authoritative. Showing
            // it prevents an inconsistent redundant RuleValue from changing
            // the predicate merely because the editor was opened and saved.
            value_entry.set_text(&amount.to_string());
        }
        if matches!(
            &rule.operator,
            RuleOperator::IsRated | RuleOperator::IsUnrated
        ) {
            value_entry.set_visible(false);
            value2_entry.set_visible(false);
        }
    }

    // Store widget names for extraction.
    field_dropdown.set_widget_name("field");
    op_dropdown.set_widget_name("operator");
    value_entry.set_widget_name("value");
    date_unit_dropdown.set_widget_name("date_unit");
    value2_entry.set_widget_name("value2");
    rating_error.set_widget_name("rating_error");

    // Revalidate after every user-editable rating component changes. Field
    // and operator handlers above run first, so visibility and operator sets
    // are already current when validation observes the row.
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        field_dropdown.connect_selected_notify(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rating_validation(&dialog, &rules_box);
        });
    }
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        op_dropdown.connect_selected_notify(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rating_validation(&dialog, &rules_box);
        });
    }
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        value_entry.connect_changed(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rating_validation(&dialog, &rules_box);
        });
    }
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        value2_entry.connect_changed(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rating_validation(&dialog, &rules_box);
        });
    }

    row
}

/// Extract a `SmartRule` from a rule row's widgets.
fn extract_rule_from_row(row: &gtk::Box) -> Option<SmartRule> {
    let mut field_dropdown: Option<gtk::DropDown> = None;
    let mut op_dropdown: Option<gtk::DropDown> = None;
    let mut value_entry: Option<gtk::Entry> = None;
    let mut date_unit_dropdown: Option<gtk::DropDown> = None;
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
        } else if name == "date_unit" {
            date_unit_dropdown = widget.downcast_ref::<gtk::DropDown>().cloned();
        } else if name == "value2" {
            value2_entry = widget.downcast_ref::<gtk::Entry>().cloned();
        }
        child = widget.next_sibling();
    }

    let field_dd = field_dropdown?;
    let op_dd = op_dropdown?;
    let val_entry = value_entry?;
    let date_unit = date_unit_dropdown
        .map(|dropdown| index_to_date_unit(dropdown.selected()))
        .unwrap_or(DateUnit::Days);

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
                4 | 5 => relative_date_operator(
                    op_dd.selected(),
                    val_text.parse::<u32>().unwrap_or(30),
                    date_unit,
                )
                .expect("relative date indexes are exhaustive"),
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
        FieldType::Rating => {
            let rule = rating_rule_from_editor(op_dd.selected(), &val_text, &val2_text).ok()?;
            (rule.operator, rule.value)
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
    "Composer",
    "Year",
    "Track Number",
    "Disc Number",
    "Genre",
    "Duration",
    "Bitrate",
    "Play Count",
    "Last Played",
    "Date Added",
    "Date Modified",
    "Track ID",
    "Rating",
];

/// Map dropdown index to `SortField`.
fn index_to_sort_field(idx: u32) -> SortField {
    match idx {
        0 => SortField::Artist,
        1 => SortField::AlbumArtist,
        2 => SortField::Album,
        3 => SortField::Title,
        4 => SortField::Composer,
        5 => SortField::Year,
        6 => SortField::TrackNumber,
        7 => SortField::DiscNumber,
        8 => SortField::Genre,
        9 => SortField::Duration,
        10 => SortField::Bitrate,
        11 => SortField::PlayCount,
        12 => SortField::LastPlayed,
        13 => SortField::DateAdded,
        14 => SortField::DateModified,
        15 => SortField::TrackId,
        16 => SortField::Rating,
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
        SortField::Composer => 4,
        SortField::Year => 5,
        SortField::TrackNumber => 6,
        SortField::DiscNumber => 7,
        SortField::Genre => 8,
        SortField::Duration => 9,
        SortField::Bitrate => 10,
        SortField::PlayCount => 11,
        SortField::LastPlayed => 12,
        SortField::DateAdded => 13,
        SortField::DateModified => 14,
        SortField::TrackId => 15,
        SortField::Rating => 16,
    }
}

/// Build a single sort criterion row with field dropdown and direction toggle.
fn build_sort_row(
    existing: Option<&SortCriterion>,
    sort_box: gtk::glib::WeakRef<gtk::Box>,
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
        let row = row.downgrade();
        remove_btn.connect_clicked(move |_| {
            let (Some(sort_box), Some(row)) = (sort_box.upgrade(), row.upgrade()) else {
                return;
            };
            sort_box.remove(&row);
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct RatingRuleCatalog {
        ratings: RatingRuleMessages,
    }

    #[derive(Debug, Deserialize)]
    struct RatingRuleMessages {
        rule_not_integer: String,
        rule_out_of_range: String,
        rule_reversed_range: String,
    }

    #[test]
    fn every_rule_field_round_trips_through_the_editor_mapping() {
        let fields = [
            RuleField::Title,
            RuleField::Artist,
            RuleField::AlbumArtist,
            RuleField::Album,
            RuleField::Genre,
            RuleField::Composer,
            RuleField::Year,
            RuleField::TrackNumber,
            RuleField::DiscNumber,
            RuleField::Duration,
            RuleField::Bitrate,
            RuleField::SampleRate,
            RuleField::Format,
            RuleField::PlayCount,
            RuleField::LastPlayed,
            RuleField::DateAdded,
            RuleField::DateModified,
            RuleField::FileSize,
            RuleField::Rating,
        ];

        for field in fields {
            assert_eq!(index_to_field(field_to_index(&field)), field);
        }
        assert_eq!(
            FIELD_NAMES[field_to_index(&RuleField::LastPlayed) as usize],
            "Last Played"
        );
        assert!(matches!(
            field_type(&RuleField::LastPlayed),
            FieldType::Date
        ));
        assert_eq!(
            FIELD_NAMES[field_to_index(&RuleField::Rating) as usize],
            "Rating (1–100)"
        );
        assert!(matches!(field_type(&RuleField::Rating), FieldType::Rating));
    }

    #[test]
    fn every_sort_field_round_trips_through_the_editor_mapping() {
        assert_eq!(
            SORT_FIELD_NAMES,
            [
                "Artist",
                "Album Artist",
                "Album",
                "Title",
                "Composer",
                "Year",
                "Track Number",
                "Disc Number",
                "Genre",
                "Duration",
                "Bitrate",
                "Play Count",
                "Last Played",
                "Date Added",
                "Date Modified",
                "Track ID",
                "Rating",
            ]
        );
        let fields = [
            SortField::TrackId,
            SortField::Artist,
            SortField::AlbumArtist,
            SortField::Album,
            SortField::Title,
            SortField::Composer,
            SortField::Year,
            SortField::TrackNumber,
            SortField::DiscNumber,
            SortField::Genre,
            SortField::Duration,
            SortField::Bitrate,
            SortField::PlayCount,
            SortField::LastPlayed,
            SortField::DateAdded,
            SortField::DateModified,
            SortField::Rating,
        ];

        for field in fields {
            assert_eq!(index_to_sort_field(sort_field_to_index(field)), field);
        }
        assert_eq!(
            SORT_FIELD_NAMES[sort_field_to_index(SortField::LastPlayed) as usize],
            "Last Played"
        );
        assert_eq!(
            SORT_FIELD_NAMES[sort_field_to_index(SortField::Rating) as usize],
            "Rating"
        );
    }

    #[test]
    fn every_limit_selection_round_trips_including_playback_recency() {
        let selections = [
            LimitSort::Random,
            LimitSort::Title,
            LimitSort::Album,
            LimitSort::Artist,
            LimitSort::Genre,
            LimitSort::Year,
            LimitSort::Bitrate,
            LimitSort::MostPlayed,
            LimitSort::LeastPlayed,
            LimitSort::MostRecentlyAdded,
            LimitSort::LeastRecentlyAdded,
            LimitSort::MostRecentlyPlayed,
            LimitSort::LeastRecentlyPlayed,
            LimitSort::HighestRated,
            LimitSort::LowestRated,
        ];

        for selection in selections {
            assert_eq!(
                index_to_limit_sort(limit_sort_to_index(selection)),
                selection
            );
        }
        assert_eq!(
            LIMIT_SORTS[limit_sort_to_index(LimitSort::MostRecentlyPlayed) as usize],
            "Most Recently Played"
        );
        assert_eq!(
            LIMIT_SORTS[limit_sort_to_index(LimitSort::LeastRecentlyPlayed) as usize],
            "Least Recently Played"
        );
        assert_eq!(
            LIMIT_SORTS[limit_sort_to_index(LimitSort::HighestRated) as usize],
            "Highest Rated"
        );
        assert_eq!(
            LIMIT_SORTS[limit_sort_to_index(LimitSort::LowestRated) as usize],
            "Lowest Rated"
        );
    }

    #[test]
    fn rating_operators_and_values_round_trip_through_editor_mappings() {
        for (index, expected) in [
            (0, "Is"),
            (1, "IsNot"),
            (2, "GreaterThan"),
            (3, "LessThan"),
            (4, "InRange"),
            (5, "IsRated"),
            (6, "IsUnrated"),
        ] {
            let operator = index_to_rating_operator(index);
            assert_eq!(rating_operator_to_index(&operator), index);
            assert_eq!(
                serde_json::to_value(operator)
                    .expect("serialize rating operator")
                    .as_str(),
                Some(expected)
            );
        }

        let range = rating_rule_from_editor(4, "20", "80").expect("canonical rating range");
        assert!(matches!(range.operator, RuleOperator::InRange));
        assert!(matches!(range.value, RuleValue::NumberRange(20, 80)));

        for index in [5, 6] {
            let presence = rating_rule_from_editor(index, "", "")
                .expect("presence predicates require no operand");
            assert!(matches!(presence.value, RuleValue::Number(1)));
        }

        assert_eq!(RATING_OPS[5..], ["is rated", "is unrated"]);
    }

    #[test]
    fn invalid_rating_editor_operands_are_rejected_without_clamping_or_guessing() {
        assert_eq!(canonical_rating_operand(" 73 "), Ok(73));

        for raw in ["", "   ", "seventy", "1.5"] {
            assert_eq!(
                rating_rule_from_editor(0, raw, "").unwrap_err(),
                RatingRuleInputError::NotAnInteger
            );
        }
        for raw in ["0", "101", "-1", "9223372036854775808"] {
            let expected = if raw == "9223372036854775808" {
                RatingRuleInputError::NotAnInteger
            } else {
                RatingRuleInputError::OutOfRange
            };
            assert_eq!(rating_rule_from_editor(0, raw, "").unwrap_err(), expected);
        }

        assert_eq!(
            rating_rule_from_editor(4, "80", "20").unwrap_err(),
            RatingRuleInputError::ReversedRange
        );
        assert_eq!(
            rating_rule_from_editor(4, "20", "").unwrap_err(),
            RatingRuleInputError::NotAnInteger
        );
        assert_eq!(
            rating_rule_from_editor(4, "20", "101").unwrap_err(),
            RatingRuleInputError::OutOfRange
        );
    }

    #[test]
    fn rating_validation_messages_are_backed_by_every_yaml_catalog() {
        let locale_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("locales");

        for locale in rust_i18n::available_locales!() {
            let path = locale_dir.join(format!("{locale}.yml"));
            let yaml = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let catalog: RatingRuleCatalog = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));

            for (error, expected) in [
                (
                    RatingRuleInputError::NotAnInteger,
                    catalog.ratings.rule_not_integer,
                ),
                (
                    RatingRuleInputError::OutOfRange,
                    catalog.ratings.rule_out_of_range,
                ),
                (
                    RatingRuleInputError::ReversedRange,
                    catalog.ratings.rule_reversed_range,
                ),
            ] {
                assert!(!expected.trim().is_empty(), "{}: {error:?}", path.display());
                assert_eq!(
                    error.message(locale.as_ref()),
                    expected,
                    "rating validation fell back instead of using {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn relative_date_operators_preserve_amount_and_unit_through_editor_mappings() {
        for unit in [DateUnit::Days, DateUnit::Weeks, DateUnit::Months] {
            let unit_index = date_unit_to_index(unit);
            assert_eq!(index_to_date_unit(unit_index), unit);

            for operator_index in [4, 5] {
                let operator = relative_date_operator(operator_index, 7, unit)
                    .expect("relative operator index");
                assert_eq!(relative_date_unit(&operator), Some(unit));
                match operator {
                    RuleOperator::IsInTheLast {
                        amount,
                        unit: restored,
                    } if operator_index == 4 => {
                        assert_eq!(amount, 7);
                        assert_eq!(restored, unit);
                    }
                    RuleOperator::IsNotInTheLast {
                        amount,
                        unit: restored,
                    } if operator_index == 5 => {
                        assert_eq!(amount, 7);
                        assert_eq!(restored, unit);
                    }
                    unexpected => panic!("unexpected relative operator: {unexpected:?}"),
                }
            }
        }

        assert_eq!(DATE_UNITS, ["days", "weeks", "months"]);
    }
}
