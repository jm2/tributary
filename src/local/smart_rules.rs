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
    /// Optional compound sort order applied to the final results.
    /// Each criterion is applied in sequence (multi-key sort).
    /// Example: Artist ascending → Year ascending → Track # ascending
    /// produces the Tauon-style "artists alphabetised, albums chronological" layout.
    #[serde(default)]
    pub sort_order: Vec<SortCriterion>,
}

/// A single sort criterion for compound playlist ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortCriterion {
    pub field: SortField,
    pub direction: SortDirection,
}

/// Fields available for compound sort ordering.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SortField {
    Artist,
    AlbumArtist,
    Album,
    Title,
    Year,
    TrackNumber,
    DiscNumber,
    Genre,
    Duration,
    Bitrate,
    PlayCount,
    DateAdded,
    DateModified,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SortDirection {
    Ascending,
    Descending,
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
    AlbumArtist,
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
    fn album_artist(&self) -> &str;
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
    fn album_artist(&self) -> &str {
        self.album_artist_name.as_deref().unwrap_or("")
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

    // Apply compound sort order if specified.
    if !rules.sort_order.is_empty() {
        apply_compound_sort(&mut results, &rules.sort_order);
    }

    // Apply limit if configured.
    if let Some(limit) = &rules.limit {
        apply_limit(&mut results, limit);
    }

    results
}

/// Apply a multi-key compound sort to the results.
///
/// Criteria are applied in order: the first criterion is the primary sort,
/// the second breaks ties in the first, etc.  This enables Tauon-style
/// generator code ordering like "Artist asc → Year asc → Track # asc".
fn apply_compound_sort<T: SmartTrack>(results: &mut [T], criteria: &[SortCriterion]) {
    if criteria.is_empty() {
        return;
    }
    results.sort_by(|a, b| {
        for criterion in criteria {
            let cmp = compare_by_sort_field(a, b, criterion.field);
            let cmp = match criterion.direction {
                SortDirection::Ascending => cmp,
                SortDirection::Descending => cmp.reverse(),
            };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Compare two tracks by a single sort field.
fn compare_by_sort_field<T: SmartTrack>(a: &T, b: &T, field: SortField) -> std::cmp::Ordering {
    match field {
        SortField::Artist => a.artist().to_lowercase().cmp(&b.artist().to_lowercase()),
        SortField::AlbumArtist => a
            .album_artist()
            .to_lowercase()
            .cmp(&b.album_artist().to_lowercase()),
        SortField::Album => a.album().to_lowercase().cmp(&b.album().to_lowercase()),
        SortField::Title => a.title().to_lowercase().cmp(&b.title().to_lowercase()),
        SortField::Year => a.year().cmp(&b.year()),
        SortField::TrackNumber => a.track_number().cmp(&b.track_number()),
        SortField::DiscNumber => a.disc_number().cmp(&b.disc_number()),
        SortField::Genre => a.genre().to_lowercase().cmp(&b.genre().to_lowercase()),
        SortField::Duration => a.duration_secs().cmp(&b.duration_secs()),
        SortField::Bitrate => a.bitrate_kbps().cmp(&b.bitrate_kbps()),
        SortField::PlayCount => a.play_count().cmp(&b.play_count()),
        SortField::DateAdded => a.date_added().cmp(b.date_added()),
        SortField::DateModified => a.date_modified().cmp(b.date_modified()),
    }
}

/// Evaluate a single rule against a track.
fn evaluate_rule<T: SmartTrack>(rule: &SmartRule, track: &T) -> bool {
    match rule.field {
        RuleField::Title => eval_text(track.title(), &rule.operator, &rule.value),
        RuleField::Artist => eval_text(track.artist(), &rule.operator, &rule.value),
        RuleField::AlbumArtist => eval_text(track.album_artist(), &rule.operator, &rule.value),
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
    let Some(field_val) = field_val else {
        return false;
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

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test track implementation ───────────────────────────────────

    #[derive(Debug, Clone)]
    struct TestTrack {
        title: String,
        artist: String,
        album: String,
        genre: String,
        year: Option<i32>,
        track_number: Option<i32>,
        disc_number: Option<i32>,
        duration_secs: Option<i64>,
        bitrate_kbps: Option<i32>,
        sample_rate_hz: Option<i32>,
        format: String,
        play_count: i32,
        date_added: String,
        date_modified: String,
        file_size_bytes: Option<i64>,
    }

    impl TestTrack {
        fn new(title: &str, artist: &str, album: &str) -> Self {
            Self {
                title: title.to_string(),
                artist: artist.to_string(),
                album: album.to_string(),
                genre: String::new(),
                year: None,
                track_number: None,
                disc_number: None,
                duration_secs: None,
                bitrate_kbps: None,
                sample_rate_hz: None,
                format: String::new(),
                play_count: 0,
                date_added: "2025-01-01T00:00:00Z".to_string(),
                date_modified: "2025-01-01T00:00:00Z".to_string(),
                file_size_bytes: None,
            }
        }
    }

    impl SmartTrack for TestTrack {
        fn title(&self) -> &str {
            &self.title
        }
        fn artist(&self) -> &str {
            &self.artist
        }
        fn album_artist(&self) -> &str {
            "" // TestTrack doesn't have album_artist
        }
        fn album(&self) -> &str {
            &self.album
        }
        fn genre(&self) -> &str {
            &self.genre
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
            &self.format
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

    // ── Text operator tests ─────────────────────────────────────────

    #[test]
    fn test_eval_text_is() {
        assert!(eval_text(
            "Rock",
            &RuleOperator::Is,
            &RuleValue::Text("Rock".into())
        ));
        assert!(eval_text(
            "rock",
            &RuleOperator::Is,
            &RuleValue::Text("ROCK".into())
        ));
        assert!(!eval_text(
            "Pop",
            &RuleOperator::Is,
            &RuleValue::Text("Rock".into())
        ));
    }

    #[test]
    fn test_eval_text_is_not() {
        assert!(eval_text(
            "Pop",
            &RuleOperator::IsNot,
            &RuleValue::Text("Rock".into())
        ));
        assert!(!eval_text(
            "Rock",
            &RuleOperator::IsNot,
            &RuleValue::Text("Rock".into())
        ));
    }

    #[test]
    fn test_eval_text_contains() {
        assert!(eval_text(
            "Progressive Rock",
            &RuleOperator::Contains,
            &RuleValue::Text("rock".into())
        ));
        assert!(!eval_text(
            "Jazz",
            &RuleOperator::Contains,
            &RuleValue::Text("rock".into())
        ));
    }

    #[test]
    fn test_eval_text_does_not_contain() {
        assert!(eval_text(
            "Jazz",
            &RuleOperator::DoesNotContain,
            &RuleValue::Text("rock".into())
        ));
        assert!(!eval_text(
            "Progressive Rock",
            &RuleOperator::DoesNotContain,
            &RuleValue::Text("rock".into())
        ));
    }

    #[test]
    fn test_eval_text_starts_with() {
        assert!(eval_text(
            "The Beatles",
            &RuleOperator::StartsWith,
            &RuleValue::Text("the".into())
        ));
        assert!(!eval_text(
            "Beatles, The",
            &RuleOperator::StartsWith,
            &RuleValue::Text("the".into())
        ));
    }

    #[test]
    fn test_eval_text_ends_with() {
        assert!(eval_text(
            "Beatles, The",
            &RuleOperator::EndsWith,
            &RuleValue::Text("the".into())
        ));
        assert!(!eval_text(
            "The Beatles",
            &RuleOperator::EndsWith,
            &RuleValue::Text("the".into())
        ));
    }

    #[test]
    fn test_eval_text_wrong_value_type() {
        // Passing a Number value to a text operator should return false.
        assert!(!eval_text(
            "Rock",
            &RuleOperator::Is,
            &RuleValue::Number(42)
        ));
    }

    // ── Numeric operator tests ──────────────────────────────────────

    #[test]
    fn test_eval_number_is() {
        assert!(eval_number(
            Some(2020),
            &RuleOperator::Is,
            &RuleValue::Number(2020)
        ));
        assert!(!eval_number(
            Some(2019),
            &RuleOperator::Is,
            &RuleValue::Number(2020)
        ));
    }

    #[test]
    fn test_eval_number_is_not() {
        assert!(eval_number(
            Some(2019),
            &RuleOperator::IsNot,
            &RuleValue::Number(2020)
        ));
        assert!(!eval_number(
            Some(2020),
            &RuleOperator::IsNot,
            &RuleValue::Number(2020)
        ));
    }

    #[test]
    fn test_eval_number_greater_than() {
        assert!(eval_number(
            Some(320),
            &RuleOperator::GreaterThan,
            &RuleValue::Number(256)
        ));
        assert!(!eval_number(
            Some(128),
            &RuleOperator::GreaterThan,
            &RuleValue::Number(256)
        ));
        assert!(!eval_number(
            Some(256),
            &RuleOperator::GreaterThan,
            &RuleValue::Number(256)
        ));
    }

    #[test]
    fn test_eval_number_less_than() {
        assert!(eval_number(
            Some(128),
            &RuleOperator::LessThan,
            &RuleValue::Number(256)
        ));
        assert!(!eval_number(
            Some(320),
            &RuleOperator::LessThan,
            &RuleValue::Number(256)
        ));
    }

    #[test]
    fn test_eval_number_in_range() {
        assert!(eval_number(
            Some(2000),
            &RuleOperator::InRange,
            &RuleValue::NumberRange(1990, 2010)
        ));
        assert!(eval_number(
            Some(1990),
            &RuleOperator::InRange,
            &RuleValue::NumberRange(1990, 2010)
        ));
        assert!(eval_number(
            Some(2010),
            &RuleOperator::InRange,
            &RuleValue::NumberRange(1990, 2010)
        ));
        assert!(!eval_number(
            Some(1989),
            &RuleOperator::InRange,
            &RuleValue::NumberRange(1990, 2010)
        ));
        assert!(!eval_number(
            Some(2011),
            &RuleOperator::InRange,
            &RuleValue::NumberRange(1990, 2010)
        ));
    }

    #[test]
    fn test_eval_number_none_returns_false() {
        assert!(!eval_number(
            None,
            &RuleOperator::Is,
            &RuleValue::Number(42)
        ));
        assert!(!eval_number(
            None,
            &RuleOperator::GreaterThan,
            &RuleValue::Number(0)
        ));
    }

    #[test]
    fn test_eval_number_wrong_value_type() {
        assert!(!eval_number(
            Some(42),
            &RuleOperator::Is,
            &RuleValue::Text("42".into())
        ));
    }

    // ── Date operator tests ─────────────────────────────────────────

    #[test]
    fn test_eval_date_is() {
        assert!(eval_date(
            "2025-06-15",
            &RuleOperator::Is,
            &RuleValue::Date("2025-06-15".into())
        ));
        assert!(!eval_date(
            "2025-06-14",
            &RuleOperator::Is,
            &RuleValue::Date("2025-06-15".into())
        ));
    }

    #[test]
    fn test_eval_date_is_before() {
        assert!(eval_date(
            "2024-01-01",
            &RuleOperator::IsBefore,
            &RuleValue::Date("2025-01-01".into())
        ));
        assert!(!eval_date(
            "2025-06-01",
            &RuleOperator::IsBefore,
            &RuleValue::Date("2025-01-01".into())
        ));
    }

    #[test]
    fn test_eval_date_is_after() {
        assert!(eval_date(
            "2025-06-01",
            &RuleOperator::IsAfter,
            &RuleValue::Date("2025-01-01".into())
        ));
        assert!(!eval_date(
            "2024-01-01",
            &RuleOperator::IsAfter,
            &RuleValue::Date("2025-01-01".into())
        ));
    }

    // ── Match mode tests ────────────────────────────────────────────

    #[test]
    fn test_evaluate_match_all() {
        let mut t = TestTrack::new("Song", "Artist", "Album");
        t.genre = "Rock".to_string();
        t.year = Some(2020);

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![
                SmartRule {
                    field: RuleField::Genre,
                    operator: RuleOperator::Is,
                    value: RuleValue::Text("Rock".into()),
                },
                SmartRule {
                    field: RuleField::Year,
                    operator: RuleOperator::Is,
                    value: RuleValue::Number(2020),
                },
            ],
            limit: None,
            live_updating: true,
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &[t.clone()]);
        assert_eq!(result.len(), 1);

        // Change year so one rule fails.
        let mut t2 = t;
        t2.year = Some(2019);
        let result = evaluate(&rules, &[t2]);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_evaluate_match_any() {
        let mut t = TestTrack::new("Song", "Artist", "Album");
        t.genre = "Jazz".to_string();
        t.year = Some(2020);

        let rules = SmartRules {
            match_mode: MatchMode::Any,
            rules: vec![
                SmartRule {
                    field: RuleField::Genre,
                    operator: RuleOperator::Is,
                    value: RuleValue::Text("Rock".into()),
                },
                SmartRule {
                    field: RuleField::Year,
                    operator: RuleOperator::Is,
                    value: RuleValue::Number(2020),
                },
            ],
            limit: None,
            live_updating: true,
            sort_order: Vec::new(),
        };

        // Genre doesn't match but year does → included.
        let result = evaluate(&rules, &[t]);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_evaluate_empty_rules_matches_all() {
        let t = TestTrack::new("Song", "Artist", "Album");
        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: None,
            live_updating: true,
            sort_order: Vec::new(),
        };
        let result = evaluate(&rules, &[t]);
        assert_eq!(result.len(), 1);
    }

    // ── Limit tests ─────────────────────────────────────────────────

    #[test]
    fn test_limit_by_items() {
        let tracks: Vec<TestTrack> = (0..10)
            .map(|i| TestTrack::new(&format!("Song {i}"), "Artist", "Album"))
            .collect();

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 5,
                unit: LimitUnit::Items,
                selected_by: LimitSort::Title,
            }),
            live_updating: true,
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &tracks);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn test_limit_by_duration() {
        let tracks: Vec<TestTrack> = (0..5)
            .map(|i| {
                let mut t = TestTrack::new(&format!("Song {i}"), "Artist", "Album");
                t.duration_secs = Some(120); // 2 minutes each
                t
            })
            .collect();

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 5, // 5 minutes = 300 seconds
                unit: LimitUnit::Minutes,
                selected_by: LimitSort::Title,
            }),
            live_updating: true,
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &tracks);
        // 2 tracks = 240s (fits), 3 tracks = 360s (exceeds 300s)
        // But the algorithm keeps adding until it would exceed, so:
        // track 0: 120 <= 300 → keep (total=120)
        // track 1: 240 <= 300 → keep (total=240)
        // track 2: 360 > 300 and keep>0 → stop
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_limit_by_size() {
        let tracks: Vec<TestTrack> = (0..5)
            .map(|i| {
                let mut t = TestTrack::new(&format!("Song {i}"), "Artist", "Album");
                t.file_size_bytes = Some(600_000); // ~600 KB each
                t
            })
            .collect();

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 1, // 1 MB = 1_048_576 bytes
                unit: LimitUnit::MB,
                selected_by: LimitSort::Title,
            }),
            live_updating: true,
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &tracks);
        // track 0: 600K <= 1M → keep
        // track 1: 1.2M > 1M → stop
        assert_eq!(result.len(), 1);
    }

    // ── Field-specific evaluation tests ─────────────────────────────

    #[test]
    fn test_evaluate_play_count() {
        let mut t = TestTrack::new("Song", "Artist", "Album");
        t.play_count = 10;

        let rule = SmartRule {
            field: RuleField::PlayCount,
            operator: RuleOperator::GreaterThan,
            value: RuleValue::Number(5),
        };
        assert!(evaluate_rule(&rule, &t));

        let rule = SmartRule {
            field: RuleField::PlayCount,
            operator: RuleOperator::GreaterThan,
            value: RuleValue::Number(15),
        };
        assert!(!evaluate_rule(&rule, &t));
    }

    #[test]
    fn test_evaluate_bitrate() {
        let mut t = TestTrack::new("Song", "Artist", "Album");
        t.bitrate_kbps = Some(320);

        let rule = SmartRule {
            field: RuleField::Bitrate,
            operator: RuleOperator::GreaterThan,
            value: RuleValue::Number(256),
        };
        assert!(evaluate_rule(&rule, &t));
    }

    #[test]
    fn test_evaluate_format() {
        let mut t = TestTrack::new("Song", "Artist", "Album");
        t.format = "FLAC".to_string();

        let rule = SmartRule {
            field: RuleField::Format,
            operator: RuleOperator::Is,
            value: RuleValue::Text("flac".into()),
        };
        assert!(evaluate_rule(&rule, &t)); // case-insensitive
    }

    // ── Leap year helper ────────────────────────────────────────────

    #[test]
    fn test_is_leap_year() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2023));
    }

    // ── Date cutoff computation ─────────────────────────────────────

    #[test]
    fn test_compute_date_cutoff_format() {
        let cutoff = compute_date_cutoff(30, DateUnit::Days);
        // Should be a valid YYYY-MM-DD string.
        assert_eq!(cutoff.len(), 10);
        assert_eq!(&cutoff[4..5], "-");
        assert_eq!(&cutoff[7..8], "-");
    }

    // ── Limit sort ordering ─────────────────────────────────────────

    #[test]
    fn test_limit_sort_by_title() {
        let tracks = vec![
            TestTrack::new("Zebra", "A", "X"),
            TestTrack::new("Apple", "B", "Y"),
            TestTrack::new("Mango", "C", "Z"),
        ];

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 2,
                unit: LimitUnit::Items,
                selected_by: LimitSort::Title,
            }),
            live_updating: true,
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &tracks);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title(), "Apple");
        assert_eq!(result[1].title(), "Mango");
    }

    #[test]
    fn test_limit_sort_most_played() {
        let mut t1 = TestTrack::new("A", "X", "Y");
        t1.play_count = 5;
        let mut t2 = TestTrack::new("B", "X", "Y");
        t2.play_count = 20;
        let mut t3 = TestTrack::new("C", "X", "Y");
        t3.play_count = 10;

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 2,
                unit: LimitUnit::Items,
                selected_by: LimitSort::MostPlayed,
            }),
            live_updating: true,
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &[t1, t2, t3]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title(), "B"); // 20 plays
        assert_eq!(result[1].title(), "C"); // 10 plays
    }

    // ── Proptest property-based tests ───────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_test_track() -> impl Strategy<Value = TestTrack> {
            (
                "[a-zA-Z ]{1,30}",               // title
                "[a-zA-Z ]{1,20}",               // artist
                "[a-zA-Z ]{1,20}",               // album
                prop::option::of(1900..2030i32), // year
                prop::option::of(0..500i64),     // duration
                0..100i32,                       // play_count
            )
                .prop_map(|(title, artist, album, year, duration, play_count)| {
                    let mut t = TestTrack::new(&title, &artist, &album);
                    t.year = year;
                    t.duration_secs = duration;
                    t.play_count = play_count;
                    t
                })
        }

        proptest! {
            #[test]
            fn all_mode_subset_of_any_mode(
                tracks in proptest::collection::vec(arb_test_track(), 0..50),
                year_threshold in 1900..2030i64,
            ) {
                let rules_all = SmartRules {
                    match_mode: MatchMode::All,
                    rules: vec![
                        SmartRule {
                            field: RuleField::Year,
                            operator: RuleOperator::GreaterThan,
                            value: RuleValue::Number(year_threshold),
                        },
                        SmartRule {
                            field: RuleField::PlayCount,
                            operator: RuleOperator::GreaterThan,
                            value: RuleValue::Number(0),
                        },
                    ],
                    limit: None,
                    live_updating: true,
                    sort_order: Vec::new(),
                };

                let rules_any = SmartRules {
                    match_mode: MatchMode::Any,
                    rules: rules_all.rules.clone(),
                    limit: None,
                    live_updating: true,
                    sort_order: Vec::new(),
                };

                let result_all = evaluate(&rules_all, &tracks);
                let result_any = evaluate(&rules_any, &tracks);

                // All-mode results must be a subset of Any-mode results.
                prop_assert!(result_all.len() <= result_any.len());
                for t in &result_all {
                    prop_assert!(result_any.iter().any(|a| a.title() == t.title()));
                }
            }

            #[test]
            fn limit_never_increases_count(
                tracks in proptest::collection::vec(arb_test_track(), 0..100),
                limit_value in 1..50u32,
            ) {
                let rules_unlimited = SmartRules {
                    match_mode: MatchMode::All,
                    rules: vec![],
                    limit: None,
                    live_updating: true,
                    sort_order: Vec::new(),
                };

                let rules_limited = SmartRules {
                    match_mode: MatchMode::All,
                    rules: vec![],
                    limit: Some(SmartLimit {
                        value: limit_value,
                        unit: LimitUnit::Items,
                        selected_by: LimitSort::Title,
                    }),
                    live_updating: true,
                    sort_order: Vec::new(),
                };

                let unlimited = evaluate(&rules_unlimited, &tracks);
                let limited = evaluate(&rules_limited, &tracks);

                prop_assert!(limited.len() <= unlimited.len());
                prop_assert!(limited.len() <= limit_value as usize);
            }

            #[test]
            fn empty_rules_returns_all_tracks(
                tracks in proptest::collection::vec(arb_test_track(), 0..50),
            ) {
                let rules = SmartRules {
                    match_mode: MatchMode::All,
                    rules: vec![],
                    limit: None,
                    live_updating: true,
                    sort_order: Vec::new(),
                };

                let result = evaluate(&rules, &tracks);
                prop_assert_eq!(result.len(), tracks.len());
            }
        }
    }
}
