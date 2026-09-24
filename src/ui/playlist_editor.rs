//! Smart playlist editor dialog — iTunes-style rule editor.
//!
//! Presents a modal `adw::AlertDialog` for creating and editing smart
//! playlist rules. Each rule row has field, operator, and value widgets
//! that dynamically update based on the selected field type.

use adw::prelude::*;

use crate::local::smart_rules::*;

// ── Field metadata ──────────────────────────────────────────────────

/// Catalog keys of the field labels, in dropdown order.
const FIELD_KEYS: &[&str] = &[
    "smart_playlist.field.title",
    "smart_playlist.field.artist",
    "smart_playlist.field.album_artist",
    "smart_playlist.field.album",
    "smart_playlist.field.genre",
    "smart_playlist.field.composer",
    "smart_playlist.field.year",
    "smart_playlist.field.track_number",
    "smart_playlist.field.disc_number",
    "smart_playlist.field.duration_seconds",
    "smart_playlist.field.bitrate_kbps",
    "smart_playlist.field.sample_rate_hz",
    "smart_playlist.field.format",
    "smart_playlist.field.play_count",
    "smart_playlist.field.last_played",
    "smart_playlist.field.date_added",
    "smart_playlist.field.date_modified",
    "smart_playlist.field.file_size_bytes",
    "smart_playlist.field.rating_range",
];

/// A dropdown model of the catalog strings `keys` names, in order.
fn string_list(keys: &[&str], locale: &str) -> gtk::StringList {
    let labels: Vec<String> = keys
        .iter()
        .map(|key| rust_i18n::t!(*key, locale = locale).into_owned())
        .collect();
    gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())
}

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

/// Text operator label keys.
const TEXT_OPS: &[&str] = &[
    "smart_playlist.operator.is",
    "smart_playlist.operator.is_not",
    "smart_playlist.operator.contains",
    "smart_playlist.operator.does_not_contain",
    "smart_playlist.operator.starts_with",
    "smart_playlist.operator.ends_with",
];

/// Numeric operator label keys.
const NUM_OPS: &[&str] = &[
    "smart_playlist.operator.is",
    "smart_playlist.operator.is_not",
    "smart_playlist.operator.greater_than",
    "smart_playlist.operator.less_than",
    "smart_playlist.operator.in_range",
];

/// Rating operators retain the numeric operator indexes and append explicit
/// presence predicates. This keeps existing editor mappings stable.
const RATING_OPS: &[&str] = &[
    "smart_playlist.operator.is",
    "smart_playlist.operator.is_not",
    "smart_playlist.operator.greater_than",
    "smart_playlist.operator.less_than",
    "smart_playlist.operator.in_range",
    "smart_playlist.operator.is_rated",
    "smart_playlist.operator.is_unrated",
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

/// Why a rule row cannot be saved as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleInputError {
    Rating(RatingRuleInputError),
    NotAnInteger,
    ReversedRange,
    NotPositive,
    InvalidDate,
}

impl RuleInputError {
    fn message(self, locale: &str) -> String {
        let key = match self {
            Self::Rating(error) => return error.message(locale),
            Self::NotAnInteger => "smart_playlist.rule_not_integer",
            Self::ReversedRange => "smart_playlist.rule_reversed_range",
            Self::NotPositive => "smart_playlist.rule_not_positive",
            Self::InvalidDate => "smart_playlist.rule_invalid_date",
        };
        rust_i18n::t!(key, locale = locale).into_owned()
    }
}

fn index_to_text_operator(idx: u32) -> RuleOperator {
    match idx {
        0 => RuleOperator::Is,
        1 => RuleOperator::IsNot,
        3 => RuleOperator::DoesNotContain,
        4 => RuleOperator::StartsWith,
        5 => RuleOperator::EndsWith,
        _ => RuleOperator::Contains,
    }
}

fn index_to_number_operator(idx: u32) -> RuleOperator {
    match idx {
        1 => RuleOperator::IsNot,
        2 => RuleOperator::GreaterThan,
        3 => RuleOperator::LessThan,
        4 => RuleOperator::InRange,
        _ => RuleOperator::Is,
    }
}

fn index_to_absolute_date_operator(idx: u32) -> RuleOperator {
    match idx {
        1 => RuleOperator::IsNot,
        2 => RuleOperator::IsBefore,
        3 => RuleOperator::IsAfter,
        _ => RuleOperator::Is,
    }
}

fn whole_number(raw: &str) -> Result<i64, RuleInputError> {
    raw.trim()
        .parse::<i64>()
        .map_err(|_| RuleInputError::NotAnInteger)
}

/// Build the rule one editor row describes. Input that is not exactly a
/// value of the row's type is refused rather than replaced by a default, so
/// a typo can never save a different predicate than the one the user wrote.
fn rule_from_editor(
    field: RuleField,
    op_index: u32,
    raw_value: &str,
    raw_high: &str,
    date_unit: DateUnit,
) -> Result<SmartRule, RuleInputError> {
    let (operator, value) = match field_type(&field) {
        FieldType::Text => (
            index_to_text_operator(op_index),
            RuleValue::Text(raw_value.to_owned()),
        ),
        FieldType::Number => {
            let operator = index_to_number_operator(op_index);
            let value = if matches!(operator, RuleOperator::InRange) {
                let low = whole_number(raw_value)?;
                let high = whole_number(raw_high)?;
                if low > high {
                    return Err(RuleInputError::ReversedRange);
                }
                RuleValue::NumberRange(low, high)
            } else {
                RuleValue::Number(whole_number(raw_value)?)
            };
            (operator, value)
        }
        FieldType::Date if is_relative_date_index(op_index) => {
            let amount = raw_value
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|amount| *amount > 0)
                .ok_or(RuleInputError::NotPositive)?;
            (
                relative_date_operator(op_index, amount, date_unit)
                    .expect("relative date indexes are exhaustive"),
                RuleValue::Number(i64::from(amount)),
            )
        }
        FieldType::Date => {
            let day = parse_rule_day(raw_value).ok_or(RuleInputError::InvalidDate)?;
            (
                index_to_absolute_date_operator(op_index),
                RuleValue::Date(day.format("%Y-%m-%d").to_string()),
            )
        }
        FieldType::Rating => {
            return rating_rule_from_editor(op_index, raw_value, raw_high)
                .map_err(RuleInputError::Rating);
        }
    };

    Ok(SmartRule {
        field,
        operator,
        value,
    })
}

/// Whether the operator at `op_index` takes a second "to" value.
fn is_range_operator(field: &RuleField, op_index: u32) -> bool {
    match field_type(field) {
        FieldType::Number => matches!(index_to_number_operator(op_index), RuleOperator::InRange),
        FieldType::Rating => matches!(index_to_rating_operator(op_index), RuleOperator::InRange),
        FieldType::Text | FieldType::Date => false,
    }
}

/// Placeholder for the value entry, which shows the expected input form.
fn value_placeholder(field: &RuleField, op_index: u32, locale: &str) -> String {
    match field_type(field) {
        FieldType::Rating => "1–100".to_owned(),
        FieldType::Date if !is_relative_date_index(op_index) => "YYYY-MM-DD".to_owned(),
        _ => rust_i18n::t!("smart_playlist.value_placeholder", locale = locale).into_owned(),
    }
}

fn set_entry_error(entry: &gtk::Entry, message: Option<&str>) {
    if let Some(message) = message {
        entry.add_css_class("error");
        entry.update_property(&[gtk::accessible::Property::Description(message)]);
    } else {
        entry.remove_css_class("error");
        entry.reset_property(gtk::AccessibleProperty::Description);
    }
}

/// The widgets of one rendered rule row, found by widget name.
struct RuleRowWidgets {
    field: gtk::DropDown,
    operator: gtk::DropDown,
    value: gtk::Entry,
    value2: gtk::Entry,
    date_unit: gtk::DropDown,
    error: gtk::Label,
}

impl RuleRowWidgets {
    fn find(row: &gtk::Box) -> Option<Self> {
        let mut field = None;
        let mut operator = None;
        let mut value = None;
        let mut value2 = None;
        let mut date_unit = None;
        let mut error = None;

        let mut child = row.first_child();
        while let Some(widget) = child {
            match widget.widget_name().as_str() {
                "field" => field = widget.downcast_ref::<gtk::DropDown>().cloned(),
                "operator" => operator = widget.downcast_ref::<gtk::DropDown>().cloned(),
                "value" => value = widget.downcast_ref::<gtk::Entry>().cloned(),
                "value2" => value2 = widget.downcast_ref::<gtk::Entry>().cloned(),
                "date_unit" => date_unit = widget.downcast_ref::<gtk::DropDown>().cloned(),
                "rule_error" => error = widget.downcast_ref::<gtk::Label>().cloned(),
                _ => {}
            }
            child = widget.next_sibling();
        }

        Some(Self {
            field: field?,
            operator: operator?,
            value: value?,
            value2: value2?,
            date_unit: date_unit?,
            error: error?,
        })
    }

    fn rule(&self) -> Result<SmartRule, RuleInputError> {
        rule_from_editor(
            index_to_field(self.field.selected()),
            self.operator.selected(),
            &self.value.text(),
            &self.value2.text(),
            index_to_date_unit(self.date_unit.selected()),
        )
    }
}

/// Validate one rendered row without changing either operand.
///
/// The visible error label and each invalid entry's accessible description
/// carry the same message. Presence predicates require no user input because
/// the editor supplies their canonical inert placeholder itself.
fn validate_rule_row(row: &gtk::Box) -> bool {
    let Some(widgets) = RuleRowWidgets::find(row) else {
        return false;
    };

    let validation = widgets.rule();
    let locale = rust_i18n::locale();
    let message = validation
        .as_ref()
        .err()
        .copied()
        .map(|error| error.message(locale.as_ref()));
    widgets
        .error
        .set_label(message.as_deref().unwrap_or_default());
    widgets.error.set_visible(message.is_some());

    let is_range = is_range_operator(
        &index_to_field(widgets.field.selected()),
        widgets.operator.selected(),
    );
    set_entry_error(&widgets.value, message.as_deref());
    set_entry_error(
        &widgets.value2,
        if is_range { message.as_deref() } else { None },
    );
    validation.is_ok()
}

/// Revalidate every row and enable OK only when all of them can be saved.
fn refresh_rule_validation(dialog: &adw::AlertDialog, rules_box: &gtk::Box) -> bool {
    let mut valid = true;
    let mut child = rules_box.first_child();
    while let Some(widget) = child {
        if let Some(row) = widget.downcast_ref::<gtk::Box>() {
            valid &= validate_rule_row(row);
        }
        child = widget.next_sibling();
    }
    dialog.set_response_enabled("ok", valid);
    valid
}

/// Date operator label keys.
const DATE_OPS: &[&str] = &[
    "smart_playlist.operator.is",
    "smart_playlist.operator.is_not",
    "smart_playlist.operator.is_before",
    "smart_playlist.operator.is_after",
    "smart_playlist.operator.is_in_the_last",
    "smart_playlist.operator.is_not_in_the_last",
];

/// Relative-date unit label keys.
const DATE_UNITS: &[&str] = &[
    "smart_playlist.date_unit.days",
    "smart_playlist.date_unit.weeks",
    "smart_playlist.date_unit.months",
];

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

/// Whether a date operator index is one of the relative "in the last" modes.
fn is_relative_date_index(op_index: u32) -> bool {
    matches!(op_index, 4 | 5)
}

fn relative_date_operator(op_index: u32, amount: u32, unit: DateUnit) -> Option<RuleOperator> {
    match op_index {
        4 => Some(RuleOperator::IsInTheLast { amount, unit }),
        5 => Some(RuleOperator::IsNotInTheLast { amount, unit }),
        _ => None,
    }
}

/// Limit unit label keys.
const LIMIT_UNITS: &[&str] = &[
    "smart_playlist.limit_unit.items",
    "smart_playlist.limit_unit.minutes",
    "smart_playlist.limit_unit.hours",
    "smart_playlist.limit_unit.megabytes",
    "smart_playlist.limit_unit.gigabytes",
];

/// Limit sort-by label keys.
const LIMIT_SORTS: &[&str] = &[
    "smart_playlist.limit_sort.random",
    "smart_playlist.field.title",
    "smart_playlist.field.album",
    "smart_playlist.field.artist",
    "smart_playlist.field.genre",
    "smart_playlist.field.year",
    "smart_playlist.field.bitrate",
    "smart_playlist.limit_sort.most_played",
    "smart_playlist.limit_sort.least_played",
    "smart_playlist.limit_sort.most_recently_added",
    "smart_playlist.limit_sort.least_recently_added",
    "smart_playlist.limit_sort.most_recently_played",
    "smart_playlist.limit_sort.least_recently_played",
    "smart_playlist.limit_sort.highest_rated",
    "smart_playlist.limit_sort.lowest_rated",
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
    let locale: std::rc::Rc<str> = std::rc::Rc::from(&*rust_i18n::locale());
    let dialog = adw::AlertDialog::builder()
        .heading(if existing_rules.is_some() {
            rust_i18n::t!(
                "smart_playlist.edit_title",
                locale = &*locale,
                name = playlist_name
            )
        } else {
            rust_i18n::t!("smart_playlist.new_title", locale = &*locale)
        })
        .close_response("cancel")
        .default_response("ok")
        .build();

    dialog.add_response(
        "cancel",
        &rust_i18n::t!("dialogs.cancel", locale = &*locale),
    );
    dialog.add_response("ok", &rust_i18n::t!("dialogs.ok", locale = &*locale));
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    let label = |key: &str| gtk::Label::new(Some(&rust_i18n::t!(key, locale = &*locale)));

    // ── Match mode ──────────────────────────────────────────────────
    let match_model = string_list(
        &["smart_playlist.match_all", "smart_playlist.match_any"],
        &locale,
    );
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
    match_row.append(&label("smart_playlist.match_label"));
    match_row.append(&match_dropdown);
    match_row.append(&label("smart_playlist.match_suffix"));

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
        let row = build_rule_row(
            Some(rule),
            &locale,
            rules_box_weak.clone(),
            dialog_weak.clone(),
        );
        rules_box.append(&row);
    }
    refresh_rule_validation(&dialog, &rules_box);

    // ── Add rule button ─────────────────────────────────────────────
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(rust_i18n::t!("smart_playlist.add_rule", locale = &*locale).as_ref())
        .build();
    {
        let rules_box = rules_box_weak.clone();
        let dialog = dialog_weak.clone();
        let locale = locale.clone();
        add_btn.connect_clicked(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            let row = build_rule_row(None, &locale, rules_box.downgrade(), dialog.downgrade());
            rules_box.append(&row);
            refresh_rule_validation(&dialog, &rules_box);
        });
    }

    // ── Limit section ───────────────────────────────────────────────
    let limit_check = gtk::CheckButton::builder()
        .label(rust_i18n::t!("smart_playlist.limit_to", locale = &*locale).as_ref())
        .active(existing_rules.is_some_and(|r| r.limit.is_some()))
        .build();

    let limit_value = gtk::SpinButton::with_range(1.0, 99999.0, 1.0);
    limit_value.set_value(
        existing_rules
            .and_then(|r| r.limit.as_ref())
            .map(|l| l.value as f64)
            .unwrap_or(25.0),
    );

    let limit_unit_model = string_list(LIMIT_UNITS, &locale);
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

    let limit_sort_model = string_list(LIMIT_SORTS, &locale);
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
    limit_row.append(&label("smart_playlist.selected_by"));
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
        .label(rust_i18n::t!("smart_playlist.sort_by", locale = &*locale).as_ref())
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
        let row = build_sort_row(Some(criterion), &locale, sort_box_weak.clone());
        sort_box.append(&row);
    }

    let add_sort_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(rust_i18n::t!("smart_playlist.add_sort_level", locale = &*locale).as_ref())
        .halign(gtk::Align::End)
        .build();
    {
        let sort_box = sort_box_weak.clone();
        let locale = locale.clone();
        add_sort_btn.connect_clicked(move |_| {
            let Some(sort_box) = sort_box.upgrade() else {
                return;
            };
            let row = build_sort_row(None, &locale, sort_box.downgrade());
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
        if !refresh_rule_validation(dialog, &rules_box) {
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
    locale: &str,
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
        op_model.append(&rust_i18n::t!(*op, locale = locale));
    }
    op_dropdown.set_selected(0);
    value.set_visible(true);
    value.set_placeholder_text(Some(&value_placeholder(&field, 0, locale)));
    value2.set_visible(false);
    date_unit.set_visible(false);
}

/// Build a single rule row with field, operator, and value widgets.
fn build_rule_row(
    existing: Option<&SmartRule>,
    locale: &str,
    rules_box: gtk::glib::WeakRef<gtk::Box>,
    dialog: gtk::glib::WeakRef<adw::AlertDialog>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();

    // Field dropdown.
    let field_model = string_list(FIELD_KEYS, locale);
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
        .placeholder_text(
            rust_i18n::t!("smart_playlist.value_placeholder", locale = locale).as_ref(),
        )
        .hexpand(true)
        .width_chars(12)
        .build();

    // Second value entry (for "in range").
    let value2_entry = gtk::Entry::builder()
        .placeholder_text(
            rust_i18n::t!("smart_playlist.range_end_placeholder", locale = locale).as_ref(),
        )
        .width_chars(8)
        .visible(false)
        .build();

    // Unit selector for relative date operators. It remains part of every
    // row so switching fields/operators cannot lose a previously selected
    // Weeks/Months value, but is visible only for the two relative modes.
    let date_unit_model = string_list(DATE_UNITS, locale);
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

    let rule_error = gtk::Label::builder()
        .css_classes(["error"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .visible(false)
        .build();
    rule_error.set_accessible_role(gtk::AccessibleRole::Alert);

    // Remove button.
    let remove_btn = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(rust_i18n::t!("smart_playlist.remove_rule", locale = locale).as_ref())
        .build();

    row.append(&field_dropdown);
    row.append(&op_dropdown);
    row.append(&value_entry);
    row.append(&date_unit_dropdown);
    row.append(&value2_entry);
    row.append(&rule_error);
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
                refresh_rule_validation(&dialog, &rules_box);
            }
        });
    }

    // Wire field dropdown to update operators.
    {
        // Initial population.
        update_rule_operator_widgets(
            field_dropdown.selected(),
            locale,
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
        let locale = locale.to_owned();
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
                &locale,
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
        let locale = locale.to_owned();
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
            value2.set_visible(is_range_operator(&field, dd.selected()));
            value.set_placeholder_text(Some(&value_placeholder(&field, dd.selected(), &locale)));
            let is_rating_presence = matches!(field_type, FieldType::Rating)
                && matches!(
                    index_to_rating_operator(dd.selected()),
                    RuleOperator::IsRated | RuleOperator::IsUnrated
                );
            value.set_visible(!is_rating_presence);
            let is_relative_date =
                matches!(field_type, FieldType::Date) && is_relative_date_index(dd.selected());
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
    rule_error.set_widget_name("rule_error");

    // Revalidate after every user-editable component changes. Field
    // and operator handlers above run first, so visibility and operator sets
    // are already current when validation observes the row.
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        field_dropdown.connect_selected_notify(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rule_validation(&dialog, &rules_box);
        });
    }
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        op_dropdown.connect_selected_notify(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rule_validation(&dialog, &rules_box);
        });
    }
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        value_entry.connect_changed(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rule_validation(&dialog, &rules_box);
        });
    }
    {
        let rules_box = rules_box.clone();
        let dialog = dialog.clone();
        value2_entry.connect_changed(move |_| {
            let (Some(rules_box), Some(dialog)) = (rules_box.upgrade(), dialog.upgrade()) else {
                return;
            };
            refresh_rule_validation(&dialog, &rules_box);
        });
    }

    row
}

/// Extract a `SmartRule` from a rule row's widgets, or `None` when the row
/// does not hold a valid rule.
fn extract_rule_from_row(row: &gtk::Box) -> Option<SmartRule> {
    RuleRowWidgets::find(row)?.rule().ok()
}

// ── Sort row builder ────────────────────────────────────────────────

/// Sort field label keys for the dropdown (must match `SortField` enum order).
const SORT_FIELD_KEYS: &[&str] = &[
    "smart_playlist.field.artist",
    "smart_playlist.field.album_artist",
    "smart_playlist.field.album",
    "smart_playlist.field.title",
    "smart_playlist.field.composer",
    "smart_playlist.field.year",
    "smart_playlist.field.track_number",
    "smart_playlist.field.disc_number",
    "smart_playlist.field.genre",
    "smart_playlist.field.duration",
    "smart_playlist.field.bitrate",
    "smart_playlist.field.play_count",
    "smart_playlist.field.last_played",
    "smart_playlist.field.date_added",
    "smart_playlist.field.date_modified",
    "smart_playlist.field.track_id",
    "smart_playlist.field.rating",
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
    locale: &str,
    sort_box: gtk::glib::WeakRef<gtk::Box>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();

    let field_model = string_list(SORT_FIELD_KEYS, locale);
    let field_dropdown = gtk::DropDown::builder()
        .model(&field_model)
        .selected(existing.map(|c| sort_field_to_index(c.field)).unwrap_or(0))
        .hexpand(true)
        .build();

    let dir_model = string_list(
        &[
            "smart_playlist.sort_direction.ascending",
            "smart_playlist.sort_direction.descending",
        ],
        locale,
    );
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
        .tooltip_text(rust_i18n::t!("smart_playlist.remove_sort_level", locale = locale).as_ref())
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

/// GTK-touching contracts folded into the crate's single consolidated
/// GTK-initializing test (browser.rs `gtk_widget_contracts_hold_on_one_session`);
/// see `ui::widget_test_session`. Mirrors the caller's macOS gate so these
/// helpers are never dead code there.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::*;

    fn rule_row_in_dialog() -> (adw::AlertDialog, gtk::Box, RuleRowWidgets) {
        let dialog = adw::AlertDialog::new(None, None);
        dialog.add_response("ok", "OK");
        let rules_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let row = build_rule_row(None, "en", rules_box.downgrade(), dialog.downgrade());
        rules_box.append(&row);
        let widgets = RuleRowWidgets::find(&row).expect("rule row widgets");
        (dialog, rules_box, widgets)
    }

    /// Invalid number and date input disables OK and shows why, instead of
    /// saving a coerced value; fixing it re-enables OK.
    pub fn number_and_date_rows_gate_ok() {
        let (dialog, rules_box, widgets) = rule_row_in_dialog();

        widgets.field.set_selected(field_to_index(&RuleField::Year));
        widgets.value.set_text("199O");
        assert!(!dialog.is_response_enabled("ok"));
        assert!(widgets.error.is_visible());
        assert!(widgets.value.has_css_class("error"));
        widgets.value.set_text("1990");
        assert!(dialog.is_response_enabled("ok"));
        assert!(!widgets.error.is_visible());
        assert!(!widgets.value.has_css_class("error"));

        widgets.operator.set_selected(4);
        assert!(WidgetExt::is_visible(&widgets.value2));
        widgets.value2.set_text("1980");
        assert!(!dialog.is_response_enabled("ok"), "reversed range");
        widgets.value2.set_text("1999");
        assert!(dialog.is_response_enabled("ok"));

        widgets
            .field
            .set_selected(field_to_index(&RuleField::DateAdded));
        assert_eq!(
            widgets.value.placeholder_text().as_deref(),
            Some("YYYY-MM-DD")
        );
        widgets.value.set_text("01/15/2024");
        assert!(!dialog.is_response_enabled("ok"));
        widgets.value.set_text("2024-01-15");
        assert!(dialog.is_response_enabled("ok"));
        assert!(refresh_rule_validation(&dialog, &rules_box));

        widgets.operator.set_selected(4);
        assert_eq!(widgets.value.placeholder_text().as_deref(), Some("value"));
        assert!(!dialog.is_response_enabled("ok"), "a date is not an amount");
        widgets.value.set_text("3");
        assert!(dialog.is_response_enabled("ok"));
    }

    fn items(dropdown: &gtk::DropDown) -> Vec<String> {
        let model = dropdown
            .model()
            .and_downcast::<gtk::StringList>()
            .expect("string list model");
        (0..model.n_items())
            .filter_map(|index| model.string(index))
            .map(String::from)
            .collect()
    }

    /// Rule and sort rows take every label from the locale they are built
    /// for, without touching the process-wide locale.
    pub fn rows_are_labelled_for_the_locale_they_are_built_for() {
        let dialog = adw::AlertDialog::new(None, None);
        let rules_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let row = build_rule_row(None, "de", rules_box.downgrade(), dialog.downgrade());
        let widgets = RuleRowWidgets::find(&row).expect("rule row widgets");
        assert_eq!(items(&widgets.field)[0], "Titel");
        assert_eq!(
            items(&widgets.operator),
            [
                "ist",
                "ist nicht",
                "enthält",
                "enthält nicht",
                "beginnt mit",
                "endet mit"
            ]
        );
        assert_eq!(widgets.value.placeholder_text().as_deref(), Some("Wert"));
        widgets.field.set_selected(field_to_index(&RuleField::Year));
        assert_eq!(items(&widgets.operator)[2], "größer als");

        let sort_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let sort_row = build_sort_row(None, "de", sort_box.downgrade());
        let direction = sort_row
            .first_child()
            .and_then(|field| field.next_sibling())
            .and_downcast::<gtk::DropDown>()
            .expect("sort direction dropdown");
        assert_eq!(items(&direction), ["Aufsteigend", "Absteigend"]);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde::Deserialize;

    use super::*;

    fn english(key: &str) -> String {
        rust_i18n::t!(key, locale = "en").into_owned()
    }

    #[derive(Debug, Deserialize)]
    struct RatingRuleCatalog {
        ratings: RatingRuleMessages,
        smart_playlist: RuleInputMessages,
    }

    #[derive(Debug, Deserialize)]
    struct RuleInputMessages {
        rule_not_integer: String,
        rule_reversed_range: String,
        rule_not_positive: String,
        rule_invalid_date: String,
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
            english(FIELD_KEYS[field_to_index(&RuleField::LastPlayed) as usize]),
            "Last Played"
        );
        assert!(matches!(
            field_type(&RuleField::LastPlayed),
            FieldType::Date
        ));
        assert_eq!(
            english(FIELD_KEYS[field_to_index(&RuleField::Rating) as usize]),
            "Rating (1–100)"
        );
        assert!(matches!(field_type(&RuleField::Rating), FieldType::Rating));
    }

    #[test]
    fn every_sort_field_round_trips_through_the_editor_mapping() {
        assert_eq!(
            SORT_FIELD_KEYS
                .iter()
                .map(|key| english(key))
                .collect::<Vec<_>>(),
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
            english(SORT_FIELD_KEYS[sort_field_to_index(SortField::LastPlayed) as usize]),
            "Last Played"
        );
        assert_eq!(
            english(SORT_FIELD_KEYS[sort_field_to_index(SortField::Rating) as usize]),
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
            english(LIMIT_SORTS[limit_sort_to_index(LimitSort::MostRecentlyPlayed) as usize]),
            "Most Recently Played"
        );
        assert_eq!(
            english(LIMIT_SORTS[limit_sort_to_index(LimitSort::LeastRecentlyPlayed) as usize]),
            "Least Recently Played"
        );
        assert_eq!(
            english(LIMIT_SORTS[limit_sort_to_index(LimitSort::HighestRated) as usize]),
            "Highest Rated"
        );
        assert_eq!(
            english(LIMIT_SORTS[limit_sort_to_index(LimitSort::LowestRated) as usize]),
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

        assert_eq!(
            RATING_OPS[5..]
                .iter()
                .map(|key| english(key))
                .collect::<Vec<_>>(),
            ["is rated", "is unrated"]
        );
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

            let messages = catalog.smart_playlist;
            for (error, expected) in [
                (RuleInputError::NotAnInteger, messages.rule_not_integer),
                (RuleInputError::ReversedRange, messages.rule_reversed_range),
                (RuleInputError::NotPositive, messages.rule_not_positive),
                (RuleInputError::InvalidDate, messages.rule_invalid_date),
            ] {
                assert!(!expected.trim().is_empty(), "{}: {error:?}", path.display());
                assert_eq!(
                    error.message(locale.as_ref()),
                    expected,
                    "rule validation fell back instead of using {}",
                    path.display()
                );
            }
        }
    }

    fn editor_rule(
        field: RuleField,
        op_index: u32,
        value: &str,
        high: &str,
    ) -> Result<SmartRule, RuleInputError> {
        rule_from_editor(field, op_index, value, high, DateUnit::Days)
    }

    #[test]
    fn number_rules_refuse_text_instead_of_saving_zero() {
        for (field, raw) in [
            (RuleField::Year, "199O"),
            (RuleField::Duration, "3:30"),
            (RuleField::PlayCount, ""),
            (RuleField::Bitrate, "320kbps"),
            (RuleField::FileSize, "1.5"),
        ] {
            for op_index in 0..=3 {
                assert_eq!(
                    editor_rule(field, op_index, raw, "").unwrap_err(),
                    RuleInputError::NotAnInteger,
                    "{field:?} {raw:?}"
                );
            }
        }

        let rule = editor_rule(RuleField::Duration, 2, " 210 ", "").expect("whole seconds");
        assert!(matches!(rule.operator, RuleOperator::GreaterThan));
        assert!(matches!(rule.value, RuleValue::Number(210)));
        let rule = editor_rule(RuleField::Year, 0, "-5", "").expect("negative is a number");
        assert!(matches!(rule.value, RuleValue::Number(-5)));
    }

    #[test]
    fn number_ranges_need_two_whole_numbers_in_order() {
        assert_eq!(
            editor_rule(RuleField::Year, 4, "1990", "").unwrap_err(),
            RuleInputError::NotAnInteger
        );
        assert_eq!(
            editor_rule(RuleField::Year, 4, "", "1999").unwrap_err(),
            RuleInputError::NotAnInteger
        );
        assert_eq!(
            editor_rule(RuleField::Year, 4, "1999", "1990").unwrap_err(),
            RuleInputError::ReversedRange
        );

        let rule = editor_rule(RuleField::Year, 4, "1990", "1999").expect("ordered range");
        assert!(matches!(rule.operator, RuleOperator::InRange));
        assert!(matches!(rule.value, RuleValue::NumberRange(1990, 1999)));
        let single = editor_rule(RuleField::Year, 4, "1990", "1990").expect("one-value range");
        assert!(matches!(single.value, RuleValue::NumberRange(1990, 1990)));
        assert!(is_range_operator(&RuleField::Year, 4));
        assert!(!is_range_operator(&RuleField::Year, 2));
    }

    #[test]
    fn relative_date_amounts_refuse_anything_but_a_positive_whole_number() {
        for raw in ["", "0", "-3", "two", "1.5", "99999999999"] {
            for op_index in [4, 5] {
                assert_eq!(
                    editor_rule(RuleField::DateAdded, op_index, raw, "").unwrap_err(),
                    RuleInputError::NotPositive,
                    "{raw:?}"
                );
            }
        }

        let rule = rule_from_editor(RuleField::LastPlayed, 4, " 14 ", "", DateUnit::Weeks)
            .expect("positive amount");
        assert!(matches!(
            rule.operator,
            RuleOperator::IsInTheLast {
                amount: 14,
                unit: DateUnit::Weeks
            }
        ));
        assert!(matches!(rule.value, RuleValue::Number(14)));
    }

    #[test]
    fn absolute_dates_must_be_year_month_day_and_are_saved_canonically() {
        for raw in ["", "01/15/2024", "15.01.2024", "2024-02-30", "last tuesday"] {
            for op_index in 0..=3 {
                assert_eq!(
                    editor_rule(RuleField::DateAdded, op_index, raw, "").unwrap_err(),
                    RuleInputError::InvalidDate,
                    "{raw:?}"
                );
            }
        }

        for (op_index, expected) in [
            (0, RuleOperator::Is),
            (1, RuleOperator::IsNot),
            (2, RuleOperator::IsBefore),
            (3, RuleOperator::IsAfter),
        ] {
            let rule = editor_rule(RuleField::DateModified, op_index, " 2024-01-15 ", "")
                .expect("ISO calendar date");
            assert_eq!(
                std::mem::discriminant(&rule.operator),
                std::mem::discriminant(&expected)
            );
            assert!(matches!(&rule.value, RuleValue::Date(day) if day == "2024-01-15"));
        }
        assert_eq!(
            value_placeholder(&RuleField::DateAdded, 0, "en"),
            "YYYY-MM-DD"
        );
        assert_eq!(value_placeholder(&RuleField::DateAdded, 4, "en"), "value");
        assert_eq!(value_placeholder(&RuleField::DateAdded, 4, "de"), "Wert");
    }

    #[test]
    fn text_rules_keep_their_value_verbatim() {
        let rule = editor_rule(RuleField::Genre, 2, " Rock ", "").expect("text is never refused");
        assert!(matches!(rule.operator, RuleOperator::Contains));
        assert!(matches!(&rule.value, RuleValue::Text(text) if text == " Rock "));
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

        assert_eq!(
            DATE_UNITS
                .iter()
                .map(|key| english(key))
                .collect::<Vec<_>>(),
            ["days", "weeks", "months"]
        );
    }
}
