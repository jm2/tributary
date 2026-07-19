//! Smart playlist rule engine — iTunes-style rule evaluation.
//!
//! Supports text, numeric, and date operators with field-specific
//! type validation. Rules are combined with AND/OR match modes,
//! and results can be limited by count, duration, or file size.

use serde::{Deserialize, Serialize};

use crate::architecture::models::{Rating, TrackRating};

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
    TrackId,
    Artist,
    AlbumArtist,
    Album,
    Title,
    Composer,
    Year,
    TrackNumber,
    DiscNumber,
    Genre,
    Duration,
    Bitrate,
    PlayCount,
    LastPlayed,
    DateAdded,
    DateModified,
    Rating,
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
    Composer,
    Year,
    TrackNumber,
    DiscNumber,
    Duration,
    Bitrate,
    SampleRate,
    Format,
    PlayCount,
    LastPlayed,
    DateAdded,
    DateModified,
    FileSize,
    Rating,
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
    // Rating-presence operators. These are deliberately distinct from
    // numeric equality so an unsupported source is not mistaken for an
    // unrated source.
    IsRated,
    IsUnrated,
}

/// Date unit for relative date operators.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
    HighestRated,
    LowestRated,
}

// ── Track adapter trait ─────────────────────────────────────────────

/// Trait for extracting metadata from a track for rule evaluation.
///
/// This decouples the rule engine from any specific track type
/// (DB model, UI TrackObject, etc).
pub trait SmartTrack {
    fn track_id(&self) -> &str;
    fn title(&self) -> &str;
    fn artist(&self) -> &str;
    fn album_artist(&self) -> &str;
    fn album(&self) -> &str;
    fn genre(&self) -> &str;
    fn composer(&self) -> &str;
    fn year(&self) -> Option<i32>;
    fn track_number(&self) -> Option<i32>;
    fn disc_number(&self) -> Option<i32>;
    fn duration_secs(&self) -> Option<i64>;
    fn bitrate_kbps(&self) -> Option<i32>;
    fn sample_rate_hz(&self) -> Option<i32>;
    fn format(&self) -> &str;
    fn play_count(&self) -> i32;
    fn last_played_at_ms(&self) -> Option<i64>;
    fn rating(&self) -> TrackRating;
    fn date_added(&self) -> &str;
    fn date_modified(&self) -> &str;
    fn file_size_bytes(&self) -> Option<i64>;
}

/// Implement `SmartTrack` for the SeaORM track model.
impl SmartTrack for crate::db::entities::track::Model {
    fn track_id(&self) -> &str {
        &self.id
    }
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
    fn composer(&self) -> &str {
        self.composer.as_deref().unwrap_or("")
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
    fn last_played_at_ms(&self) -> Option<i64> {
        self.last_played_at_ms
    }
    fn rating(&self) -> TrackRating {
        TrackRating::writable(self.rating.and_then(|value| Rating::try_from(value).ok()))
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
/// Returns the matching tracks after applying the three evaluation stages in
/// order: filter, limit selection/membership, then final compound ordering.
pub fn evaluate<T: SmartTrack + Clone>(rules: &SmartRules, tracks: &[T]) -> Vec<T> {
    evaluate_at(rules, tracks, chrono::Utc::now())
}

/// Evaluate rules using one immutable clock snapshot.
///
/// Capturing time once keeps every relative-date predicate in an evaluation on
/// the same inclusive boundary, even when a large library takes long enough to
/// cross a millisecond or day boundary while it is being filtered.
fn evaluate_at<T: SmartTrack + Clone>(
    rules: &SmartRules,
    tracks: &[T],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<T> {
    // Filter tracks through rules.
    let mut results: Vec<T> = tracks
        .iter()
        .filter(|track| {
            let matches: Vec<bool> = rules
                .rules
                .iter()
                .map(|rule| evaluate_rule_at(rule, *track, now))
                .collect();

            // Boundary semantics for an empty rules vector are intentionally
            // asymmetric: `All` is vacuously true (matches every track) while
            // `Any` is vacuously false (matches none), mirroring AND/OR over
            // zero terms.
            match rules.match_mode {
                MatchMode::All => matches.iter().all(|m| *m),
                MatchMode::Any => matches.iter().any(|m| *m),
            }
        })
        .cloned()
        .collect();

    if results.is_empty() {
        return results;
    }

    // Select membership before applying the presentation order. A limit's
    // `selected_by` sort decides which tracks make the cut; `sort_order`
    // independently decides how that selected subset is displayed.
    if let Some(limit) = &rules.limit {
        apply_limit(&mut results, limit);
    }

    // Apply the final compound presentation order to the selected subset.
    if !rules.sort_order.is_empty() {
        apply_compound_sort(&mut results, &rules.sort_order);
    }

    results
}

/// Apply a multi-key compound sort to the results.
///
/// Criteria are applied in order: the first criterion is the primary sort,
/// the second breaks ties in the first, etc.  This enables Tauon-style
/// generator code ordering like "Artist asc → Year asc → Track # asc".
///
/// Uses decorate-sort-undecorate: each track's per-criterion comparison keys
/// (including lowercased text keys) are computed once up front, instead of
/// re-lowercasing both operands on every `sort_by` comparison (which would be
/// O(N log N) Unicode-folding allocations for compound sorts).
fn apply_compound_sort<T: SmartTrack>(results: &mut Vec<T>, criteria: &[SortCriterion]) {
    if criteria.is_empty() {
        return;
    }

    // Decorate: precompute the comparison keys for each track once.
    let mut decorated: Vec<(Vec<SortKey>, T)> = results
        .drain(..)
        .map(|track| {
            let keys = criteria.iter().map(|c| sort_key(&track, c.field)).collect();
            (keys, track)
        })
        .collect();

    let needs_rating_tie_breaker = criteria
        .iter()
        .any(|criterion| criterion.field == SortField::Rating);

    decorated.sort_by(|a, b| {
        for (idx, criterion) in criteria.iter().enumerate() {
            let cmp = compare_sort_keys(&a.0[idx], &b.0[idx], criterion.direction);
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        if needs_rating_tie_breaker {
            a.1.track_id().cmp(b.1.track_id())
        } else {
            std::cmp::Ordering::Equal
        }
    });

    // Undecorate: drop the keys and keep the sorted tracks.
    results.extend(decorated.into_iter().map(|(_, track)| track));
}

/// A precomputed comparison key for a single sort field.
///
/// Text fields are stored lowercased (case-insensitive compare); date fields
/// keep their raw RFC3339 string (lexicographic compare); numeric fields keep
/// their optional integer value (`None` sorts first, matching `Option` order).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Text(String),
    Int(Option<i64>),
    /// Nullable playback timestamps always put unknown values last, in either
    /// direction. Out-of-range Unix millisecond values are normalized to
    /// unknown before reaching this key.
    LastPlayed(Option<i64>),
    /// Ratings put unrated and unsupported tracks last in either direction.
    Rating(Option<i64>),
}

fn compare_sort_keys(
    left: &SortKey,
    right: &SortKey,
    direction: SortDirection,
) -> std::cmp::Ordering {
    match (left, right) {
        (SortKey::LastPlayed(left), SortKey::LastPlayed(right))
        | (SortKey::Rating(left), SortKey::Rating(right)) => {
            return compare_optional_i64_null_last(*left, *right, direction);
        }
        _ => {}
    }

    let ordering = left.cmp(right);
    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn compare_optional_i64_null_last(
    left: Option<i64>,
    right: Option<i64>,
    direction: SortDirection,
) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match direction {
            SortDirection::Ascending => left.cmp(&right),
            SortDirection::Descending => right.cmp(&left),
        },
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Build the comparison key for a track's value in a single sort field.
fn sort_key<T: SmartTrack>(track: &T, field: SortField) -> SortKey {
    match field {
        SortField::TrackId => SortKey::Text(track.track_id().to_string()),
        SortField::Artist => SortKey::Text(track.artist().to_lowercase()),
        SortField::AlbumArtist => SortKey::Text(track.album_artist().to_lowercase()),
        SortField::Album => SortKey::Text(track.album().to_lowercase()),
        SortField::Title => SortKey::Text(track.title().to_lowercase()),
        SortField::Genre => SortKey::Text(track.genre().to_lowercase()),
        SortField::Composer => SortKey::Text(track.composer().to_lowercase()),
        SortField::Year => SortKey::Int(track.year().map(i64::from)),
        SortField::TrackNumber => SortKey::Int(track.track_number().map(i64::from)),
        SortField::DiscNumber => SortKey::Int(track.disc_number().map(i64::from)),
        SortField::Duration => SortKey::Int(track.duration_secs()),
        SortField::Bitrate => SortKey::Int(track.bitrate_kbps().map(i64::from)),
        SortField::PlayCount => SortKey::Int(Some(i64::from(track.play_count()))),
        SortField::LastPlayed => SortKey::LastPlayed(valid_last_played_ms(track)),
        SortField::DateAdded => SortKey::Text(track.date_added().to_string()),
        SortField::DateModified => SortKey::Text(track.date_modified().to_string()),
        SortField::Rating => SortKey::Rating(rating_value(track).map(i64::from)),
    }
}

/// Evaluate a single rule against a track.
fn evaluate_rule_at<T: SmartTrack>(
    rule: &SmartRule,
    track: &T,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match rule.field {
        RuleField::Title => eval_text(track.title(), &rule.operator, &rule.value),
        RuleField::Artist => eval_text(track.artist(), &rule.operator, &rule.value),
        RuleField::AlbumArtist => eval_text(track.album_artist(), &rule.operator, &rule.value),
        RuleField::Album => eval_text(track.album(), &rule.operator, &rule.value),
        RuleField::Genre => eval_text(track.genre(), &rule.operator, &rule.value),
        RuleField::Composer => eval_text(track.composer(), &rule.operator, &rule.value),
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
        RuleField::Rating => eval_rating(track.rating(), &rule.operator, &rule.value),
        RuleField::LastPlayed => {
            // Playback history is persisted at millisecond precision. Compare it
            // against a clock at that same precision so the inclusive lower
            // boundary does not lose its final representable millisecond when
            // `Utc::now()` carries fractional-millisecond nanoseconds. Keep the
            // original clock for RFC3339 date-added/modified fields below.
            let history_now = chrono::DateTime::from_timestamp_millis(now.timestamp_millis())
                .expect("a valid UTC instant remains representable at millisecond precision");
            eval_optional_instant(
                track
                    .last_played_at_ms()
                    .and_then(chrono::DateTime::from_timestamp_millis),
                &rule.operator,
                &rule.value,
                history_now,
            )
        }
        RuleField::FileSize => eval_number(track.file_size_bytes(), &rule.operator, &rule.value),
        RuleField::DateAdded => eval_date_at(track.date_added(), &rule.operator, &rule.value, now),
        RuleField::DateModified => {
            eval_date_at(track.date_modified(), &rule.operator, &rule.value, now)
        }
    }
}

#[cfg(test)]
fn evaluate_rule<T: SmartTrack>(rule: &SmartRule, track: &T) -> bool {
    evaluate_rule_at(rule, track, chrono::Utc::now())
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

/// Evaluate a rating without collapsing an unsupported source into the
/// readable-but-unrated state.
///
/// Numeric predicates accept only canonical 1..=100 operands. A missing
/// readable value satisfies no numeric predicate, including `IsNot`; callers
/// must use `IsUnrated` when they mean absence. Unsupported ratings satisfy
/// neither numeric nor presence predicates. Presence predicates validate the
/// exact canonical inert `Number(1)` placeholder before otherwise ignoring it.
fn eval_rating(rating: TrackRating, op: &RuleOperator, value: &RuleValue) -> bool {
    if !canonical_rating_rule_value(op, value) {
        return false;
    }

    let readable_value = match rating {
        TrackRating::Unsupported => return false,
        TrackRating::ReadOnly { value } | TrackRating::Writable { value } => value,
    };

    match op {
        RuleOperator::IsRated => readable_value.is_some(),
        RuleOperator::IsUnrated => readable_value.is_none(),
        RuleOperator::Is
        | RuleOperator::IsNot
        | RuleOperator::GreaterThan
        | RuleOperator::LessThan
        | RuleOperator::InRange => {
            let Some(rating) = readable_value else {
                return false;
            };
            eval_number(Some(i64::from(rating.value())), op, value)
        }
        _ => false,
    }
}

fn canonical_rating_rule_value(op: &RuleOperator, value: &RuleValue) -> bool {
    let canonical = |value: i64| (i64::from(Rating::MIN)..=i64::from(Rating::MAX)).contains(&value);

    match (op, value) {
        (
            RuleOperator::Is
            | RuleOperator::IsNot
            | RuleOperator::GreaterThan
            | RuleOperator::LessThan,
            RuleValue::Number(value),
        ) => canonical(*value),
        (RuleOperator::InRange, RuleValue::NumberRange(low, high)) => {
            canonical(*low) && canonical(*high) && low <= high
        }
        (RuleOperator::IsRated | RuleOperator::IsUnrated, RuleValue::Number(1)) => true,
        _ => false,
    }
}

/// Evaluate a date field.
///
/// # Semantics
///
/// A track's `date_added`/`date_modified` is an **instant** — RFC3339 with an
/// offset, e.g. `2025-01-15T10:30:00+00:00`. A rule's date is a **calendar
/// day** picked in the editor, e.g. `2025-01-15`, and is interpreted as the
/// whole UTC day `[00:00:00, next 00:00:00)`.
///
/// These used to be compared as raw strings, which meant an instant was never
/// equal to a day: `"2025-01-15T10:30:00+00:00" == "2025-01-15"` is false, so
/// "Date Added **is** 2025-01-15" matched **zero tracks, forever**. `IsAfter`
/// had the mirror-image bug — the longer string sorted greater than its own
/// date prefix, so a track added *on* the boundary day counted as "after" it.
///
/// Both sides are now parsed. An unparseable instant or rule date makes the
/// rule fail to match rather than match everything.
fn eval_date_at(
    field_val: &str,
    op: &RuleOperator,
    value: &RuleValue,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    eval_optional_instant(parse_track_instant(field_val), op, value, now)
}

/// Evaluate a parsed instant. A missing or unrepresentable timestamp is
/// unknown and therefore never satisfies a predicate, including negative
/// predicates such as `IsNot` and `IsNotInTheLast`.
fn eval_optional_instant(
    instant: Option<chrono::DateTime<chrono::Utc>>,
    op: &RuleOperator,
    value: &RuleValue,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(instant) = instant else {
        return false;
    };
    match op {
        RuleOperator::Is => {
            rule_day(value).is_some_and(|(start, end)| instant >= start && instant < end)
        }
        RuleOperator::IsNot => {
            rule_day(value).is_some_and(|(start, end)| instant < start || instant >= end)
        }
        RuleOperator::IsBefore => rule_day(value).is_some_and(|(start, _)| instant < start),
        // "After 15 Jan" means after the whole of 15 Jan, not after its first
        // instant — a track added at noon that day is not "after" it.
        RuleOperator::IsAfter => rule_day(value).is_some_and(|(_, end)| instant >= end),
        RuleOperator::IsInTheLast { amount, unit } => {
            // A window too large to represent reaches back past any possible
            // track. The upper bound is also inclusive: a future timestamp is
            // not evidence that a track was played within the past window.
            instant <= now
                && date_cutoff_from(now, *amount, *unit).is_none_or(|cutoff| instant >= cutoff)
        }
        RuleOperator::IsNotInTheLast { amount, unit } => {
            instant <= now
                && date_cutoff_from(now, *amount, *unit).is_some_and(|cutoff| instant < cutoff)
        }
        _ => false,
    }
}

#[cfg(test)]
fn eval_date(field_val: &str, op: &RuleOperator, value: &RuleValue) -> bool {
    eval_date_at(field_val, op, value, chrono::Utc::now())
}

/// Parse a track timestamp, which is stored as RFC3339 with an offset.
fn parse_track_instant(field_val: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(field_val)
        .ok()
        .map(|instant| instant.with_timezone(&chrono::Utc))
}

/// Resolve a rule's calendar day to the half-open UTC instant range it covers.
fn rule_day(
    value: &RuleValue,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let RuleValue::Date(raw) = value else {
        return None;
    };
    let day = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;

    let start = day.and_hms_opt(0, 0, 0)?.and_utc();
    let end = day.succ_opt()?.and_hms_opt(0, 0, 0)?.and_utc();
    Some((start, end))
}

/// The instant N days/weeks/months before now.
///
/// Months are treated as 30-day windows for parity with how the editor presents
/// the option (a calendar-aware "previous month" subtraction is not what users
/// expect from "in the last 3 months").
///
/// Returns `None` when the window is too large to represent. The arithmetic is
/// checked throughout: `amount` is a `u32` straight from the editor, and the
/// old `Duration::days(i64::from(amount) * 30)` could push the subtraction past
/// chrono's representable range and panic.
fn date_cutoff_from(
    now: chrono::DateTime<chrono::Utc>,
    amount: u32,
    unit: DateUnit,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let days_per_unit: i64 = match unit {
        DateUnit::Days => 1,
        DateUnit::Weeks => 7,
        DateUnit::Months => 30,
    };

    let days_ago = i64::from(amount).checked_mul(days_per_unit)?;
    let window = chrono::TimeDelta::try_days(days_ago)?;
    now.checked_sub_signed(window)
}

#[cfg(test)]
fn date_cutoff(amount: u32, unit: DateUnit) -> Option<chrono::DateTime<chrono::Utc>> {
    date_cutoff_from(chrono::Utc::now(), amount, unit)
}

/// Normalize a nullable playback timestamp for sorting. Chrono rejects Unix
/// millisecond values outside its representable range; treating those as
/// unknown keeps corrupt metadata out of recency ordering.
fn valid_last_played_ms<T: SmartTrack>(track: &T) -> Option<i64> {
    let timestamp = track.last_played_at_ms()?;
    chrono::DateTime::from_timestamp_millis(timestamp).map(|_| timestamp)
}

fn rating_value<T: SmartTrack>(track: &T) -> Option<u8> {
    track.rating().value().map(Rating::value)
}

/// Apply result limiting: sort then truncate.
fn apply_limit<T: SmartTrack>(results: &mut Vec<T>, limit: &SmartLimit) {
    // Sort by the selected criteria.
    match limit.selected_by {
        LimitSort::Random => {
            // Genuinely random shuffle, re-seeded per evaluation. (A
            // `DefaultHasher`-based ordering would be fully deterministic
            // across runs and biased by the title/artist hash.)
            fastrand::shuffle(results);
        }
        // Text sorts are case-insensitive, consistent with the compound
        // sort path (`sort_key`).
        LimitSort::Title => {
            results.sort_by_cached_key(|t| t.title().to_lowercase());
        }
        LimitSort::Album => {
            results.sort_by_cached_key(|t| t.album().to_lowercase());
        }
        LimitSort::Artist => {
            results.sort_by_cached_key(|t| t.artist().to_lowercase());
        }
        LimitSort::Genre => {
            results.sort_by_cached_key(|t| t.genre().to_lowercase());
        }
        LimitSort::Year => results.sort_by_key(|t| t.year()),
        LimitSort::Bitrate => {
            results.sort_by_key(|t| std::cmp::Reverse(t.bitrate_kbps()));
        }
        LimitSort::MostPlayed => {
            results.sort_by(|left, right| {
                right
                    .play_count()
                    .cmp(&left.play_count())
                    .then_with(|| {
                        compare_optional_i64_null_last(
                            valid_last_played_ms(left),
                            valid_last_played_ms(right),
                            SortDirection::Descending,
                        )
                    })
                    .then_with(|| left.track_id().cmp(right.track_id()))
            });
        }
        LimitSort::LeastPlayed => {
            results.sort_by(|left, right| {
                left.play_count()
                    .cmp(&right.play_count())
                    .then_with(|| left.track_id().cmp(right.track_id()))
            });
        }
        LimitSort::MostRecentlyAdded => {
            results.sort_by(|a, b| b.date_added().cmp(a.date_added()));
        }
        LimitSort::LeastRecentlyAdded => {
            results.sort_by(|a, b| a.date_added().cmp(b.date_added()));
        }
        LimitSort::MostRecentlyPlayed => {
            results.sort_by(|left, right| {
                compare_optional_i64_null_last(
                    valid_last_played_ms(left),
                    valid_last_played_ms(right),
                    SortDirection::Descending,
                )
                .then_with(|| left.track_id().cmp(right.track_id()))
            });
        }
        LimitSort::LeastRecentlyPlayed => {
            results.sort_by(|left, right| {
                compare_optional_i64_null_last(
                    valid_last_played_ms(left),
                    valid_last_played_ms(right),
                    SortDirection::Ascending,
                )
                .then_with(|| left.track_id().cmp(right.track_id()))
            });
        }
        LimitSort::HighestRated => {
            results.sort_by(|left, right| {
                compare_optional_i64_null_last(
                    rating_value(left).map(i64::from),
                    rating_value(right).map(i64::from),
                    SortDirection::Descending,
                )
                .then_with(|| left.track_id().cmp(right.track_id()))
            });
        }
        LimitSort::LowestRated => {
            results.sort_by(|left, right| {
                compare_optional_i64_null_last(
                    rating_value(left).map(i64::from),
                    rating_value(right).map(i64::from),
                    SortDirection::Ascending,
                )
                .then_with(|| left.track_id().cmp(right.track_id()))
            });
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
///
/// The `keep > 0` guard guarantees at least one track is retained even when
/// the first track alone exceeds the cap, so a 0-minute/0-hour limit still
/// yields one track (unlike `LimitUnit::Items`, where a 0 limit yields none).
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
///
/// As with `truncate_by_duration`, the `keep > 0` guard always retains at
/// least one track, so a 0-MB/0-GB limit still yields one track.
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
        id: String,
        title: String,
        artist: String,
        album: String,
        genre: String,
        composer: String,
        year: Option<i32>,
        track_number: Option<i32>,
        disc_number: Option<i32>,
        duration_secs: Option<i64>,
        bitrate_kbps: Option<i32>,
        sample_rate_hz: Option<i32>,
        format: String,
        play_count: i32,
        last_played_at_ms: Option<i64>,
        rating: TrackRating,
        date_added: String,
        date_modified: String,
        file_size_bytes: Option<i64>,
    }

    impl TestTrack {
        fn new(title: &str, artist: &str, album: &str) -> Self {
            Self {
                id: title.to_string(),
                title: title.to_string(),
                artist: artist.to_string(),
                album: album.to_string(),
                genre: String::new(),
                composer: String::new(),
                year: None,
                track_number: None,
                disc_number: None,
                duration_secs: None,
                bitrate_kbps: None,
                sample_rate_hz: None,
                format: String::new(),
                play_count: 0,
                last_played_at_ms: None,
                rating: TrackRating::writable(None),
                date_added: "2025-01-01T00:00:00Z".to_string(),
                date_modified: "2025-01-01T00:00:00Z".to_string(),
                file_size_bytes: None,
            }
        }
    }

    impl SmartTrack for TestTrack {
        fn track_id(&self) -> &str {
            &self.id
        }
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
        fn composer(&self) -> &str {
            &self.composer
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
        fn last_played_at_ms(&self) -> Option<i64> {
            self.last_played_at_ms
        }
        fn rating(&self) -> TrackRating {
            self.rating
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

    // ── Rating operator tests ──────────────────────────────────────

    fn rating(value: u8) -> Rating {
        Rating::new(value).expect("canonical test rating")
    }

    #[test]
    fn rating_numeric_predicates_require_a_readable_value_and_canonical_operand() {
        let rated = TrackRating::read_only(Some(rating(75)));
        assert!(eval_rating(
            rated,
            &RuleOperator::Is,
            &RuleValue::Number(75)
        ));
        assert!(eval_rating(
            rated,
            &RuleOperator::IsNot,
            &RuleValue::Number(50)
        ));
        assert!(eval_rating(
            rated,
            &RuleOperator::GreaterThan,
            &RuleValue::Number(74)
        ));
        assert!(eval_rating(
            rated,
            &RuleOperator::InRange,
            &RuleValue::NumberRange(75, 100)
        ));

        for invalid in [0, 101, i64::MIN, i64::MAX] {
            assert!(!eval_rating(
                rated,
                &RuleOperator::IsNot,
                &RuleValue::Number(invalid)
            ));
        }
        for invalid_range in [
            RuleValue::NumberRange(0, 75),
            RuleValue::NumberRange(75, 101),
            RuleValue::NumberRange(90, 10),
        ] {
            assert!(!eval_rating(rated, &RuleOperator::InRange, &invalid_range));
        }

        for absent in [
            TrackRating::writable(None),
            TrackRating::read_only(None),
            TrackRating::unsupported(),
        ] {
            assert!(!eval_rating(
                absent,
                &RuleOperator::IsNot,
                &RuleValue::Number(50)
            ));
        }
    }

    #[test]
    fn rating_presence_predicates_distinguish_unrated_from_unsupported() {
        for rated in [
            TrackRating::writable(Some(rating(1))),
            TrackRating::read_only(Some(rating(100))),
        ] {
            assert!(eval_rating(
                rated,
                &RuleOperator::IsRated,
                &RuleValue::Number(1)
            ));
            assert!(!eval_rating(
                rated,
                &RuleOperator::IsUnrated,
                &RuleValue::Number(1)
            ));
        }

        for unrated in [TrackRating::writable(None), TrackRating::read_only(None)] {
            assert!(!eval_rating(
                unrated,
                &RuleOperator::IsRated,
                &RuleValue::Number(1)
            ));
            assert!(eval_rating(
                unrated,
                &RuleOperator::IsUnrated,
                &RuleValue::Number(1)
            ));
        }

        for operator in [RuleOperator::IsRated, RuleOperator::IsUnrated] {
            assert!(!eval_rating(
                TrackRating::unsupported(),
                &operator,
                &RuleValue::Number(1)
            ));
        }
    }

    #[test]
    fn rating_presence_predicates_require_the_canonical_inert_placeholder() {
        for (operator, matching_rating) in [
            (
                RuleOperator::IsRated,
                TrackRating::writable(Some(rating(50))),
            ),
            (RuleOperator::IsUnrated, TrackRating::read_only(None)),
        ] {
            for malformed in [
                RuleValue::Number(0),
                RuleValue::Number(2),
                RuleValue::Number(100),
                RuleValue::Text("1".into()),
                RuleValue::NumberRange(1, 1),
            ] {
                assert!(
                    !eval_rating(matching_rating, &operator, &malformed),
                    "{operator:?} accepted malformed placeholder {malformed:?}"
                );
            }
        }
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
    //
    // Every field value below is RFC3339 with a time component, because that is
    // what `tracks.date_added` actually holds. The previous tests passed
    // date-only strings on both sides — a shape production never produces —
    // which is exactly why the string-comparison bugs survived.

    fn day(value: &str) -> RuleValue {
        RuleValue::Date(value.into())
    }

    /// The headline bug: "Date Added is <day>" used to match nothing at all,
    /// because an instant is never string-equal to a bare date.
    #[test]
    fn a_date_is_rule_matches_any_instant_during_that_day() {
        for instant in [
            "2025-06-15T00:00:00+00:00",
            "2025-06-15T10:30:00+00:00",
            "2025-06-15T23:59:59+00:00",
        ] {
            assert!(
                eval_date(instant, &RuleOperator::Is, &day("2025-06-15")),
                "{instant} falls on 2025-06-15"
            );
        }

        for instant in ["2025-06-14T23:59:59+00:00", "2025-06-16T00:00:00+00:00"] {
            assert!(!eval_date(instant, &RuleOperator::Is, &day("2025-06-15")));
        }
    }

    #[test]
    fn a_date_is_not_rule_is_the_exact_complement() {
        assert!(!eval_date(
            "2025-06-15T10:30:00+00:00",
            &RuleOperator::IsNot,
            &day("2025-06-15")
        ));
        assert!(eval_date(
            "2025-06-16T00:00:00+00:00",
            &RuleOperator::IsNot,
            &day("2025-06-15")
        ));
    }

    #[test]
    fn a_date_is_before_rule_excludes_the_boundary_day_itself() {
        assert!(eval_date(
            "2024-12-31T23:59:59+00:00",
            &RuleOperator::IsBefore,
            &day("2025-01-01")
        ));
        // Midnight on the day itself is not before the day.
        assert!(!eval_date(
            "2025-01-01T00:00:00+00:00",
            &RuleOperator::IsBefore,
            &day("2025-01-01")
        ));
    }

    /// The mirror-image bug: `"2025-01-01T00:00:00+00:00" > "2025-01-01"` is
    /// true as a string, so a track added *on* the boundary day used to count
    /// as "after" it.
    #[test]
    fn a_date_is_after_rule_excludes_the_boundary_day_itself() {
        assert!(eval_date(
            "2025-01-02T00:00:00+00:00",
            &RuleOperator::IsAfter,
            &day("2025-01-01")
        ));
        for instant in ["2025-01-01T00:00:00+00:00", "2025-01-01T23:59:59+00:00"] {
            assert!(
                !eval_date(instant, &RuleOperator::IsAfter, &day("2025-01-01")),
                "{instant} is during 2025-01-01, not after it"
            );
        }
    }

    #[test]
    fn a_date_rule_offset_is_normalized_before_comparison() {
        // 2025-06-15T23:00:00-02:00 is 2025-06-16T01:00:00Z — the next UTC day.
        assert!(eval_date(
            "2025-06-15T23:00:00-02:00",
            &RuleOperator::Is,
            &day("2025-06-16")
        ));
    }

    #[test]
    fn an_unparseable_instant_or_rule_date_matches_nothing() {
        assert!(!eval_date(
            "not a timestamp",
            &RuleOperator::Is,
            &day("2025-06-15")
        ));
        assert!(!eval_date(
            "2025-06-15T10:30:00+00:00",
            &RuleOperator::Is,
            &day("the fifteenth")
        ));
        // A rule holding the wrong value variant must not match either.
        assert!(!eval_date(
            "2025-06-15T10:30:00+00:00",
            &RuleOperator::Is,
            &RuleValue::Number(2025)
        ));
    }

    #[test]
    fn a_relative_window_includes_recent_tracks_and_excludes_old_ones() {
        let now = chrono::Utc::now();
        let recent = (now - chrono::TimeDelta::try_days(3).expect("3 days")).to_rfc3339();
        let old = (now - chrono::TimeDelta::try_days(90).expect("90 days")).to_rfc3339();

        let last_week = RuleOperator::IsInTheLast {
            amount: 7,
            unit: DateUnit::Days,
        };
        assert!(eval_date(
            &recent,
            &last_week,
            &RuleValue::Text(String::new())
        ));
        assert!(!eval_date(
            &old,
            &last_week,
            &RuleValue::Text(String::new())
        ));

        let not_last_week = RuleOperator::IsNotInTheLast {
            amount: 7,
            unit: DateUnit::Days,
        };
        assert!(!eval_date(
            &recent,
            &not_last_week,
            &RuleValue::Text(String::new())
        ));
        assert!(eval_date(
            &old,
            &not_last_week,
            &RuleValue::Text(String::new())
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

    #[test]
    fn recently_played_uses_one_inclusive_clock_and_stable_track_id_ties() {
        // The production clock normally carries sub-millisecond precision while
        // playback history does not. This fractional instant proves that the
        // exact stored cutoff millisecond remains included and cutoff - 1 ms is
        // excluded.
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-18T12:00:00.123456789Z")
            .expect("fixed clock")
            .with_timezone(&chrono::Utc);
        let cutoff = now - chrono::TimeDelta::days(14);

        let mut newest = TestTrack::new("newest", "Artist", "Album");
        newest.id = "newest".into();
        newest.last_played_at_ms = Some(now.timestamp_millis());

        let mut tie_b = TestTrack::new("tie b", "Artist", "Album");
        tie_b.id = "b".into();
        tie_b.last_played_at_ms = Some(cutoff.timestamp_millis());

        let mut tie_a = TestTrack::new("tie a", "Artist", "Album");
        tie_a.id = "a".into();
        tie_a.last_played_at_ms = Some(cutoff.timestamp_millis());

        let mut too_old = TestTrack::new("old", "Artist", "Album");
        too_old.last_played_at_ms = Some(cutoff.timestamp_millis() - 1);

        let mut future = TestTrack::new("future", "Artist", "Album");
        future.last_played_at_ms = Some(now.timestamp_millis() + 1);

        let never = TestTrack::new("never", "Artist", "Album");
        let mut corrupt = TestTrack::new("corrupt", "Artist", "Album");
        corrupt.last_played_at_ms = Some(i64::MAX);

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![SmartRule {
                field: RuleField::LastPlayed,
                operator: RuleOperator::IsInTheLast {
                    amount: 14,
                    unit: DateUnit::Days,
                },
                value: RuleValue::Number(14),
            }],
            limit: None,
            sort_order: vec![
                SortCriterion {
                    field: SortField::LastPlayed,
                    direction: SortDirection::Descending,
                },
                SortCriterion {
                    field: SortField::TrackId,
                    direction: SortDirection::Ascending,
                },
            ],
        };

        let result = evaluate_at(
            &rules,
            &[too_old, tie_b, never, newest, corrupt, tie_a, future],
            now,
        );
        let ids: Vec<_> = result.iter().map(SmartTrack::track_id).collect();
        assert_eq!(ids, ["newest", "a", "b"]);
    }

    #[test]
    fn recently_played_is_empty_when_history_is_empty_or_unknown() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-18T12:00:00Z")
            .expect("fixed clock")
            .with_timezone(&chrono::Utc);
        let never = TestTrack::new("never", "Artist", "Album");
        let mut corrupt = TestTrack::new("corrupt", "Artist", "Album");
        corrupt.last_played_at_ms = Some(i64::MAX);
        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![SmartRule {
                field: RuleField::LastPlayed,
                operator: RuleOperator::IsInTheLast {
                    amount: 14,
                    unit: DateUnit::Days,
                },
                value: RuleValue::Number(14),
            }],
            limit: None,
            sort_order: Vec::new(),
        };

        assert!(evaluate_at(&rules, &[never, corrupt], now).is_empty());
    }

    #[test]
    fn top_25_order_includes_legacy_null_times_and_caps_membership() {
        let mut tied_b = TestTrack::new("tied b", "Artist", "Album");
        tied_b.id = "b".into();
        tied_b.play_count = 100;
        tied_b.last_played_at_ms = Some(200);

        let mut tied_a = TestTrack::new("tied a", "Artist", "Album");
        tied_a.id = "a".into();
        tied_a.play_count = 100;
        tied_a.last_played_at_ms = Some(200);

        let mut older = TestTrack::new("older", "Artist", "Album");
        older.id = "c".into();
        older.play_count = 100;
        older.last_played_at_ms = Some(100);

        let mut legacy = TestTrack::new("legacy", "Artist", "Album");
        legacy.id = "d".into();
        legacy.play_count = 100;
        legacy.last_played_at_ms = None;

        let mut tracks = vec![legacy, tied_b, older, tied_a];
        for count in (1..=22).rev() {
            let mut track = TestTrack::new(&format!("count {count}"), "Artist", "Album");
            track.id = format!("low-{count:02}");
            track.play_count = count;
            tracks.push(track);
        }
        let mut never_played = TestTrack::new("never", "Artist", "Album");
        never_played.play_count = 0;
        tracks.push(never_played);

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![SmartRule {
                field: RuleField::PlayCount,
                operator: RuleOperator::GreaterThan,
                value: RuleValue::Number(0),
            }],
            limit: Some(SmartLimit {
                value: 25,
                unit: LimitUnit::Items,
                selected_by: LimitSort::MostPlayed,
            }),
            sort_order: vec![
                SortCriterion {
                    field: SortField::PlayCount,
                    direction: SortDirection::Descending,
                },
                SortCriterion {
                    field: SortField::LastPlayed,
                    direction: SortDirection::Descending,
                },
                SortCriterion {
                    field: SortField::TrackId,
                    direction: SortDirection::Ascending,
                },
            ],
        };

        let result = evaluate(&rules, &tracks);
        assert_eq!(result.len(), 25);
        let ids: Vec<_> = result.iter().map(SmartTrack::track_id).collect();
        assert_eq!(&ids[..4], &["a", "b", "c", "d"]);
        assert!(
            ids.contains(&"d"),
            "legacy positive/null-time track is retained"
        );
        assert!(
            !ids.contains(&"low-01"),
            "the lowest positive count is capped"
        );
        assert!(!ids.contains(&"never"), "zero-count tracks do not qualify");
    }

    #[test]
    fn recent_play_limit_sorts_keep_unknown_timestamps_last() {
        let mut newest = TestTrack::new("newest", "Artist", "Album");
        newest.last_played_at_ms = Some(300);
        let mut oldest = TestTrack::new("oldest", "Artist", "Album");
        oldest.last_played_at_ms = Some(100);
        let never = TestTrack::new("never", "Artist", "Album");
        let mut corrupt = TestTrack::new("corrupt", "Artist", "Album");
        corrupt.last_played_at_ms = Some(i64::MAX);
        let tracks = [never, newest, corrupt, oldest];

        for (selected_by, expected) in [
            (
                LimitSort::MostRecentlyPlayed,
                ["newest", "oldest", "corrupt", "never"],
            ),
            (
                LimitSort::LeastRecentlyPlayed,
                ["oldest", "newest", "corrupt", "never"],
            ),
        ] {
            let rules = SmartRules {
                match_mode: MatchMode::All,
                rules: vec![],
                limit: Some(SmartLimit {
                    value: 4,
                    unit: LimitUnit::Items,
                    selected_by,
                }),
                sort_order: Vec::new(),
            };
            let result = evaluate(&rules, &tracks);
            let actual: Vec<_> = result.iter().map(SmartTrack::track_id).collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn rating_compound_sort_is_null_last_in_both_directions_with_stable_id_ties() {
        let mut tied_b = TestTrack::new("tied b", "Artist", "Album");
        tied_b.id = "b".into();
        tied_b.rating = TrackRating::read_only(Some(rating(80)));
        let mut tied_a = TestTrack::new("tied a", "Artist", "Album");
        tied_a.id = "a".into();
        tied_a.rating = TrackRating::writable(Some(rating(80)));
        let mut low = TestTrack::new("low", "Artist", "Album");
        low.id = "low".into();
        low.rating = TrackRating::writable(Some(rating(20)));
        let mut unrated = TestTrack::new("unrated", "Artist", "Album");
        unrated.id = "unrated".into();
        let mut unsupported = TestTrack::new("unsupported", "Artist", "Album");
        unsupported.id = "unsupported".into();
        unsupported.rating = TrackRating::unsupported();
        let tracks = [unrated, tied_b, unsupported, low, tied_a];

        for (direction, expected) in [
            (
                SortDirection::Ascending,
                ["low", "a", "b", "unrated", "unsupported"],
            ),
            (
                SortDirection::Descending,
                ["a", "b", "low", "unrated", "unsupported"],
            ),
        ] {
            let rules = SmartRules {
                match_mode: MatchMode::All,
                rules: vec![],
                limit: None,
                sort_order: vec![SortCriterion {
                    field: SortField::Rating,
                    direction,
                }],
            };
            let actual: Vec<_> = evaluate(&rules, &tracks)
                .iter()
                .map(|track| track.track_id().to_string())
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn rating_limit_selection_uses_ratings_for_membership_and_stable_id_ties() {
        let mut tied_b = TestTrack::new("tied b", "Artist", "Album");
        tied_b.id = "b".into();
        tied_b.rating = TrackRating::writable(Some(rating(80)));
        let mut tied_a = TestTrack::new("tied a", "Artist", "Album");
        tied_a.id = "a".into();
        tied_a.rating = TrackRating::writable(Some(rating(80)));
        let mut low = TestTrack::new("low", "Artist", "Album");
        low.id = "low".into();
        low.rating = TrackRating::writable(Some(rating(20)));
        let unrated = TestTrack::new("unrated", "Artist", "Album");
        let tracks = [unrated, tied_b, low, tied_a];

        for (selected_by, expected) in [
            (LimitSort::HighestRated, ["a", "b"]),
            (LimitSort::LowestRated, ["low", "a"]),
        ] {
            let rules = SmartRules {
                match_mode: MatchMode::All,
                rules: vec![],
                limit: Some(SmartLimit {
                    value: 2,
                    unit: LimitUnit::Items,
                    selected_by,
                }),
                sort_order: Vec::new(),
            };
            let actual: Vec<_> = evaluate(&rules, &tracks)
                .iter()
                .map(|track| track.track_id().to_string())
                .collect();
            assert_eq!(actual, expected);
        }
    }

    // ── Date cutoff computation ─────────────────────────────────────

    #[test]
    fn a_cutoff_window_is_measured_from_now() {
        let cutoff = date_cutoff(30, DateUnit::Days).expect("30 days is representable");
        let elapsed = chrono::Utc::now() - cutoff;
        assert_eq!(elapsed.num_days(), 30);
    }

    #[test]
    fn equivalent_windows_agree() {
        // 7 days vs 1 week must land on the same instant, to the second.
        let days = date_cutoff(7, DateUnit::Days).expect("7 days");
        let weeks = date_cutoff(1, DateUnit::Weeks).expect("1 week");
        assert!((days - weeks).num_seconds().abs() <= 1);
    }

    /// `Duration::days(i64::from(amount) * 30)` used to be able to push the
    /// subtraction past chrono's representable range and panic. `amount` comes
    /// straight from the editor as a `u32`.
    #[test]
    fn an_absurd_window_saturates_instead_of_panicking() {
        assert!(date_cutoff(u32::MAX, DateUnit::Months).is_none());

        // And a rule with such a window matches everything rather than blowing
        // up, because it reaches back past any possible track.
        let forever = RuleOperator::IsInTheLast {
            amount: u32::MAX,
            unit: DateUnit::Months,
        };
        assert!(eval_date(
            "1990-01-01T00:00:00+00:00",
            &forever,
            &RuleValue::Text(String::new())
        ));
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
            sort_order: Vec::new(),
        };

        let result = evaluate(&rules, &[t1, t2, t3]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title(), "B"); // 20 plays
        assert_eq!(result[1].title(), "C"); // 10 plays
    }

    #[test]
    fn filtering_and_item_selection_happen_before_final_sort() {
        let mut excluded = TestTrack::new("Able", "Artist", "Album");
        excluded.genre = "Jazz".into();
        excluded.play_count = 100;

        let mut most_played = TestTrack::new("Zulu", "Artist", "Album");
        most_played.genre = "Rock".into();
        most_played.play_count = 30;

        let mut second_most_played = TestTrack::new("Apple", "Artist", "Album");
        second_most_played.genre = "Rock".into();
        second_most_played.play_count = 20;

        let mut omitted_by_limit = TestTrack::new("Middle", "Artist", "Album");
        omitted_by_limit.genre = "Rock".into();
        omitted_by_limit.play_count = 10;

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![SmartRule {
                field: RuleField::Genre,
                operator: RuleOperator::Is,
                value: RuleValue::Text("Rock".into()),
            }],
            limit: Some(SmartLimit {
                value: 2,
                unit: LimitUnit::Items,
                selected_by: LimitSort::MostPlayed,
            }),
            sort_order: vec![SortCriterion {
                field: SortField::Title,
                direction: SortDirection::Ascending,
            }],
        };

        let result = evaluate(
            &rules,
            &[excluded, omitted_by_limit, most_played, second_most_played],
        );
        let titles: Vec<_> = result.iter().map(SmartTrack::title).collect();
        assert_eq!(titles, ["Apple", "Zulu"]);
    }

    #[test]
    fn capacity_selection_happens_before_final_sort() {
        let mut most_played = TestTrack::new("Zulu", "Artist", "Album");
        most_played.play_count = 30;
        most_played.duration_secs = Some(180);

        let mut second_most_played = TestTrack::new("Apple", "Artist", "Album");
        second_most_played.play_count = 20;
        second_most_played.duration_secs = Some(120);

        let mut omitted_by_capacity = TestTrack::new("Middle", "Artist", "Album");
        omitted_by_capacity.play_count = 10;
        omitted_by_capacity.duration_secs = Some(120);

        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 5,
                unit: LimitUnit::Minutes,
                selected_by: LimitSort::MostPlayed,
            }),
            sort_order: vec![SortCriterion {
                field: SortField::Title,
                direction: SortDirection::Ascending,
            }],
        };

        let result = evaluate(
            &rules,
            &[omitted_by_capacity, most_played, second_most_played],
        );
        let titles: Vec<_> = result.iter().map(SmartTrack::title).collect();
        assert_eq!(titles, ["Apple", "Zulu"]);
    }

    #[test]
    fn a_random_limited_subset_still_gets_its_final_sort() {
        let tracks: Vec<_> = [
            "Juliet", "Delta", "Alpha", "India", "Echo", "Hotel", "Bravo", "Golf", "Charlie",
            "Foxtrot",
        ]
        .into_iter()
        .map(|title| TestTrack::new(title, "Artist", "Album"))
        .collect();
        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: Some(SmartLimit {
                value: 4,
                unit: LimitUnit::Items,
                selected_by: LimitSort::Random,
            }),
            sort_order: vec![SortCriterion {
                field: SortField::Title,
                direction: SortDirection::Ascending,
            }],
        };

        let result = evaluate(&rules, &tracks);
        assert_eq!(result.len(), 4);
        assert!(result
            .windows(2)
            .all(|pair| pair[0].title().to_lowercase() <= pair[1].title().to_lowercase()));
    }

    #[test]
    fn compound_sort_honors_each_keys_direction() {
        fn track(title: &str, artist: &str, year: i32, track_number: i32) -> TestTrack {
            let mut track = TestTrack::new(title, artist, "Album");
            track.year = Some(year);
            track.track_number = Some(track_number);
            track
        }

        let tracks = vec![
            track("beta-old", "Beta", 2020, 1),
            track("alpha-old", "Alpha", 2020, 2),
            track("alpha-new-2", "Alpha", 2024, 2),
            track("beta-new", "Beta", 2024, 1),
            track("alpha-new-1", "Alpha", 2024, 1),
        ];
        let rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![],
            limit: None,
            sort_order: vec![
                SortCriterion {
                    field: SortField::Artist,
                    direction: SortDirection::Ascending,
                },
                SortCriterion {
                    field: SortField::Year,
                    direction: SortDirection::Descending,
                },
                SortCriterion {
                    field: SortField::TrackNumber,
                    direction: SortDirection::Ascending,
                },
            ],
        };

        let result = evaluate(&rules, &tracks);
        let titles: Vec<_> = result.iter().map(SmartTrack::title).collect();
        assert_eq!(
            titles,
            [
                "alpha-new-1",
                "alpha-new-2",
                "alpha-old",
                "beta-new",
                "beta-old",
            ]
        );
    }

    #[test]
    fn legacy_live_updating_is_accepted_but_not_reserialized() {
        let legacy = r#"{
            "match_mode": "All",
            "rules": [],
            "limit": null,
            "live_updating": false
        }"#;

        let rules: SmartRules = serde_json::from_str(legacy).expect("legacy rules deserialize");
        assert_eq!(rules.match_mode, MatchMode::All);
        assert!(rules.rules.is_empty());
        assert!(rules.limit.is_none());
        assert!(rules.sort_order.is_empty());

        let serialized = serde_json::to_value(&rules).expect("rules serialize");
        assert!(serialized.get("live_updating").is_none());
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
                    sort_order: Vec::new(),
                };

                let rules_any = SmartRules {
                    match_mode: MatchMode::Any,
                    rules: rules_all.rules.clone(),
                    limit: None,
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
                    sort_order: Vec::new(),
                };

                let result = evaluate(&rules, &tracks);
                prop_assert_eq!(result.len(), tracks.len());
            }
        }
    }
}
