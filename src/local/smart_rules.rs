//! Smart playlist rule engine — iTunes-style rule evaluation.
//!
//! Supports text, numeric, and date operators with field-specific
//! type validation. Rules are combined with AND/OR match modes,
//! and results can be limited by count, duration, or file size.

use serde::{Deserialize, Serialize};

// ── Data types ──────────────────────────────────────────────────────

/// A complete smart playlist rule configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartRules {
    /// How to combine rules: all (AND) or any (OR).
    pub match_mode: MatchMode,
    /// The individual filter rules.
    pub rules: Vec<SmartRule>,
    /// Optional result limiting.
    pub limit: Option<SmartLimit>,
    /// Whether the playlist auto-updates when the library changes.
    pub live_updating: bool,
}

/// Match mode for combining rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MatchMode {
    All,
    Any,
}

/// A single rule: field + operator + value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartRule {
    pub field: RuleField,
    pub operator: RuleOperator,
    pub value: RuleValue,
}

/// Filterable fields available in Tributary's metadata.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum RuleField {
    Title,
    Artist,
    Album,
    Genre,
    Year,
    TrackNumber,
    DiscNumber,
    Duration,
    Bitrate,
    SampleRate,
    Format,
    PlayCount,
    DateAdded,
    DateModified,
    FileSize,
}

/// Operators for filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleOperator {
    // Text operators
    Is,
    IsNot,
    Contains,
    DoesNotContain,
    StartsWith,
    EndsWith,
    // Numeric operators
    GreaterThan,
    LessThan,
    InRange,
    // Date operators
    IsBefore,
    IsAfter,
    IsInTheLast { amount: u32, unit: DateUnit },
    IsNotInTheLast { amount: u32, unit: DateUnit },
}

/// Date unit for relative date operators.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum DateUnit {
    Days,
    Weeks,
    Months,
}

/// The value to compare against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleValue {
    Text(String),
    Number(i64),
    NumberRange(i64, i64),
    Date(String),
    Duration(u64),
    Size(u64),
}

/// Result limiter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartLimit {
    pub value: u32,
    pub unit: LimitUnit,
    pub selected_by: LimitSort,
}

/// Units for limiting playlist size.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LimitUnit {
    Items,
    Minutes,
    Hours,
    MB,
    GB,
}

/// How to select items when limiting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LimitSort {
    Random,
    Title,
    Album,
    Artist,
    Genre,
    Year,
    Bitrate,
    MostPlayed,
    LeastPlayed,
    MostRecentlyAdded,
    LeastRecentlyAdded,
    MostRecentlyPlayed,
    LeastRecentlyPlayed,
}

// ── Track adapter trait ─────────────────────────────────────────────

/// Trait for extracting metadata from a track for rule evaluation.
///
/// This decouples the rule engine from any specific track type
/// (DB model, UI TrackObject, etc).
pub trait SmartTrack {
    fn title(&self) -> &str;
    fn artist(&self) -> &str;
    fn album(&self) -> &str;
    fn genre(&self) -> &str;
    fn year(&self) -> Option<i32>;
    fn track_number(&self) -> Option<i32>;
    fn disc_number(&self) -> Option<i32>;
    fn duration_secs(&self) -> Option<i64>;
    fn bitrate_kbps(&self) -> Option<i32>;
    fn sample_rate_hz(&self) -> Option<i32>;
    fn format(&self) -> &str;
    fn play_count(&self) -> i32;
    fn date_added(&self) -> &str;
    fn date_modified(&self) -> &str;
    fn file_size_bytes(&self) -> Option<i64>;
}

/// Implement `SmartTrack` for the SeaORM track model.
impl SmartTrack for crate::db::entities::track::Model {
    fn title(&self) -> &str {
        &self.title
    }
    fn artist(&self) -> &str {
        &self.artist_name
    }
    fn album(&self) -> &str {
        &self.album_title
    }
    fn genre(&self) -> &str {
        self.genre.as_deref().unwrap_or("")
    }
    fn year(&self) -> Option<i32> {
        self.year
    }
    fn track_number(&self) -> Option<i32> {
        self.track_number
    }
    fn disc_number(&self) -> Option<i32> {
        self.disc_number
    }
    fn duration_secs(&self) -> Option<i64> {
        self.duration_secs
    }
    fn bitrate_kbps(&self) -> Option<i32> {
        self.bitrate_kbps
    }
    fn sample_rate_hz(&self) -> Option<i32> {
        self.sample_rate_hz
    }
    fn format(&self) -> &str {
        self.format.as_deref().unwrap_or("")
    }
    fn play_count(&self) -> i32 {
        self.play_count
    }
    fn date_added(&self) -> &str {
        &self.date_added
    }
    fn date_modified(&self) -> &str {
        &self.date_modified
    }
    fn file_size_bytes(&self) -> Option<i64> {
        self.file_size_bytes
    }
}

// ── Evaluation ──────────────────────────────────────────────────────

/// Evaluate a smart playlist's rules against a set of tracks.
///
/// Returns the matching tracks, optionally limited and sorted.
pub fn evaluate<T: SmartTrack + Clone>(rules: &SmartRules, tracks: &[T]) -> Vec<T> {
    // Filter tracks through rules.
    let mut results: Vec<T> = tracks
        .iter()
        .filter(|track| {
            let matches: Vec<bool> = rules
                .rules
                .iter()
                .map(|rule| evaluate_rule(rule, *track))
                .collect();

            match rules.match_mode {
                MatchMode::All => matches.iter().all(|m| *m),
                MatchMode::Any => matches.iter().any(|m| *m),
            }
        })
        .cloned()
        .collect();

    // Apply limit if configured.
    if let Some(limit) = &rules.limit {
        apply_limit(&mut results, limit);
    }

    results
}

/// Evaluate a single rule against a track.
fn evaluate_rule<T: SmartTrack>(rule: &SmartRule, track: &T) -> bool {
    match rule.field {
        RuleField::Title => eval_text(track.title(), &rule.operator, &rule.value),
        RuleField::Artist => eval_text(track.artist(), &rule.operator, &rule.value),
        RuleField::Album => eval_text(track.album(), &rule.operator, &rule.value),
        RuleField::Genre => eval_text(track.genre(), &rule.operator, &rule.value),
        RuleField::Format => eval_text(track.format(), &rule.operator, &rule.value),
        RuleField::Year => eval_number(track.year().map(|v| v as i64), &rule.operator, &rule.value),
        RuleField::TrackNumber => eval_number(
            track.track_number().map(|v| v as i64),
            &rule.operator,
            &rule.value,
        ),
        RuleField::DiscNumber => eval_number(
            track.disc_number().map(|v| v as i64),
            &rule.operator,
            &rule.value,
        ),
        RuleField::Duration => eval_number(track.duration_secs(), &rule.operator, &rule.value),
        RuleField::Bitrate => eval_number(
            track.bitrate_kbps().map(|v| v as i64),
            &rule.operator,
            &rule.value,
        ),
        RuleField::SampleRate => eval_number(
            track.sample_rate_hz().map(|v| v as i64),
            &rule.operator,
            &rule.value,
        ),
        RuleField::PlayCount => {
            eval_number(Some(track.play_count() as i64), &rule.operator, &rule.value)
        }
        RuleField::FileSize => eval_number(track.file_size_bytes(), &rule.operator, &rule.value),
        RuleField::DateAdded => eval_date(track.date_added(), &rule.operator, &rule.value),
        RuleField::DateModified => eval_date(track.date_modified(), &rule.operator, &rule.value),
    }
}

/// Evaluate a text field against a text operator.
fn eval_text(field_val: &str, op: &RuleOperator, value: &RuleValue) -> bool {
    let target = match value {
        RuleValue::Text(s) => s.as_str(),
        _ => return false,
    };
    let field_lower = field_val.to_lowercase();
    let target_lower = target.to_lowercase();

    match op {
        RuleOperator::Is => field_lower == target_lower,
        RuleOperator::IsNot => field_lower != target_lower,
        RuleOperator::Contains => field_lower.contains(&target_lower),
        RuleOperator::DoesNotContain => !field_lower.contains(&target_lower),
        RuleOperator::StartsWith => field_lower.starts_with(&target_lower),
        RuleOperator::EndsWith => field_lower.ends_with(&target_lower),
        _ => false,
    }
}

/// Evaluate a numeric field.
fn eval_number(field_val: Option<i64>, op: &RuleOperator, value: &RuleValue) -> bool {
    let field_val = match field_val {
        Some(v) => v,
        None => return false,
    };

    match op {
        RuleOperator::Is => {
            if let RuleValue::Number(n) = value {
                field_val == *n
            } else {
                false
            }
        }
        RuleOperator::IsNot => {
            if let RuleValue::Number(n) = value {
                field_val != *n
            } else {
                false
            }
        }
        RuleOperator::GreaterThan => {
            if let RuleValue::Number(n) = value {
                field_val > *n
            } else {
                false
            }
        }
        RuleOperator::LessThan => {
            if let RuleValue::Number(n) = value {
                field_val < *n
            } else {
                false
            }
        }
        RuleOperator::InRange => {
            if let RuleValue::NumberRange(lo, hi) = value {
                field_val >= *lo && field_val <= *hi
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Evaluate a date field (RFC3339 string comparison).
fn eval_date(field_val: &str, op: &RuleOperator, value: &RuleValue) -> bool {
    match op {
        RuleOperator::Is => {
            if let RuleValue::Date(d) = value {
                field_val == d
            } else {
                false
            }
        }
        RuleOperator::IsNot => {
            if let RuleValue::Date(d) = value {
                field_val != d
            } else {
                false
            }
        }
        RuleOperator::IsBefore => {
            if let RuleValue::Date(d) = value {
                field_val < d.as_str()
            } else {
                false
            }
        }
        RuleOperator::IsAfter => {
            if let RuleValue::Date(d) = value {
                field_val > d.as_str()
            } else {
                false
            }
        }
        RuleOperator::IsInTheLast { amount, unit } => {
            let cutoff = compute_date_cutoff(*amount, *unit);
            field_val >= cutoff.as_str()
        }
        RuleOperator::IsNotInTheLast { amount, unit } => {
            let cutoff = compute_date_cutoff(*amount, *unit);
            field_val < cutoff.as_str()
        }
        _ => false,
    }
}

/// Compute the date string N days/weeks/months ago from now.
fn compute_date_cutoff(amount: u32, unit: DateUnit) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let secs_ago: u64 = match unit {
        DateUnit::Days => amount as u64 * 86400,
        DateUnit::Weeks => amount as u64 * 604800,
        DateUnit::Months => amount as u64 * 2592000, // ~30 days
    };

    let cutoff = now.saturating_sub(secs_ago);

    // Convert epoch seconds to a simple ISO-8601 date string.
    // We use a basic calculation since we don't have chrono.
    let secs_per_day = 86400u64;
    let days = cutoff / secs_per_day;
    // Approximate: days since 1970-01-01
    let mut year = 1970i32;
    let mut remaining_days = days as i32;

    loop {
        let year_days = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        year += 1;
    }

    let month_days: [i32; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        month += 1;
    }

    let day = remaining_days + 1;
    format!("{year:04}-{month:02}-{day:02}")
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Apply result limiting: sort then truncate.
fn apply_limit<T: SmartTrack>(results: &mut Vec<T>, limit: &SmartLimit) {
    // Sort by the selected criteria.
    match limit.selected_by {
        LimitSort::Random => {
            // Simple pseudo-random shuffle using track metadata hash.
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            results.sort_by(|a, b| {
                let mut ha = DefaultHasher::new();
                a.title().hash(&mut ha);
                a.artist().hash(&mut ha);
                let mut hb = DefaultHasher::new();
                b.title().hash(&mut hb);
                b.artist().hash(&mut hb);
                ha.finish().cmp(&hb.finish())
            });
        }
        LimitSort::Title => results.sort_by(|a, b| a.title().cmp(b.title())),
        LimitSort::Album => results.sort_by(|a, b| a.album().cmp(b.album())),
        LimitSort::Artist => results.sort_by(|a, b| a.artist().cmp(b.artist())),
        LimitSort::Genre => results.sort_by(|a, b| a.genre().cmp(b.genre())),
        LimitSort::Year => results.sort_by_key(|t| t.year()),
        LimitSort::Bitrate => {
            results.sort_by_key(|t| std::cmp::Reverse(t.bitrate_kbps()));
        }
        LimitSort::MostPlayed => {
            results.sort_by_key(|t| std::cmp::Reverse(t.play_count()));
        }
        LimitSort::LeastPlayed => {
            results.sort_by_key(|t| t.play_count());
        }
        LimitSort::MostRecentlyAdded => {
            results.sort_by(|a, b| b.date_added().cmp(a.date_added()));
        }
        LimitSort::LeastRecentlyAdded => {
            results.sort_by(|a, b| a.date_added().cmp(b.date_added()));
        }
        LimitSort::MostRecentlyPlayed | LimitSort::LeastRecentlyPlayed => {
            // No last_played field yet — fall through to no-op sort.
        }
    }

    // Truncate based on limit unit.
    let max = limit.value as usize;
    match limit.unit {
        LimitUnit::Items => {
            results.truncate(max);
        }
        LimitUnit::Minutes => {
            let max_secs = max as i64 * 60;
            truncate_by_duration(results, max_secs);
        }
        LimitUnit::Hours => {
            let max_secs = max as i64 * 3600;
            truncate_by_duration(results, max_secs);
        }
        LimitUnit::MB => {
            let max_bytes = max as i64 * 1_048_576;
            truncate_by_size(results, max_bytes);
        }
        LimitUnit::GB => {
            let max_bytes = max as i64 * 1_073_741_824;
            truncate_by_size(results, max_bytes);
        }
    }
}

/// Keep tracks until total duration exceeds `max_secs`.
fn truncate_by_duration<T: SmartTrack>(results: &mut Vec<T>, max_secs: i64) {
    let mut total = 0i64;
    let mut keep = 0;
    for track in results.iter() {
        let dur = track.duration_secs().unwrap_or(0);
        if total + dur > max_secs && keep > 0 {
            break;
        }
        total += dur;
        keep += 1;
    }
    results.truncate(keep);
}

/// Keep tracks until total file size exceeds `max_bytes`.
fn truncate_by_size<T: SmartTrack>(results: &mut Vec<T>, max_bytes: i64) {
    let mut total = 0i64;
    let mut keep = 0;
    for track in results.iter() {
        let size = track.file_size_bytes().unwrap_or(0);
        if total + size > max_bytes && keep > 0 {
            break;
        }
        total += size;
        keep += 1;
    }
    results.truncate(keep);
}
