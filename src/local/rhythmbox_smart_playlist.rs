//! Conservative translation of Rhythmbox automatic playlists.
//!
//! Rhythmbox and Tributary have overlapping smart-playlist models, but they
//! do not have identical text folding, missing-value, Boolean, or limiting
//! semantics. This module therefore recognizes one closed, lossless subset
//! and rejects the complete playlist when any source construct falls outside
//! it. It never drops an unknown predicate or approximates a query.

use std::sync::OnceLock;

use thiserror::Error;

use super::rhythmbox_import::{
    is_membership_inert_playlist_attribute, RhythmboxAutomaticNode, RhythmboxAutomaticPlaylist,
    RhythmboxRating, RhythmboxXmlAttribute,
};
use super::smart_rules::{MatchMode, RuleField, RuleOperator, RuleValue, SmartRule, SmartRules};

/// A content-free reason why an automatic playlist cannot be represented
/// exactly by Tributary's current smart-rule model.
///
/// Variants deliberately carry no source strings or values. Callers may log
/// or aggregate them without exposing playlist names, query text, or metadata.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum RhythmboxSmartPlaylistUnsupported {
    #[error("the automatic playlist has an unsupported attribute")]
    PlaylistAttribute,
    #[error("the automatic playlist uses an unsupported limit kind")]
    LimitKind,
    #[error("the automatic playlist has an invalid item limit")]
    LimitValue,
    #[error("the automatic playlist limit ordering cannot be represented exactly")]
    LimitOrdering,
    #[error("the automatic playlist has an invalid sort configuration")]
    SortConfiguration,
    #[error("the automatic playlist sort tie-break cannot be represented exactly")]
    SortTieBreak,
    #[error("the automatic playlist has an unsupported query root")]
    QueryRoot,
    #[error("the automatic playlist has an invalid outer query shape")]
    OuterShape,
    #[error("the automatic playlist does not have the exact song-type guard")]
    SongTypeGuard,
    #[error("the automatic playlist has an invalid subquery shape")]
    SubqueryShape,
    #[error("the automatic playlist contains no predicates")]
    EmptyQuery,
    #[error("the automatic playlist has a nested or mixed Boolean shape")]
    BooleanShape,
    #[error("the automatic playlist has an invalid Boolean separator")]
    BooleanSeparator,
    #[error("the automatic playlist has an invalid predicate shape")]
    PredicateShape,
    #[error("the automatic playlist uses an unsupported property")]
    PredicateProperty,
    #[error("the automatic playlist uses an unsupported operator")]
    PredicateOperator,
    #[error("the automatic playlist predicate has an invalid value")]
    PredicateValue,
    #[error("the automatic playlist predicate value is outside the exact target range")]
    PredicateRange,
    #[error("the automatic playlist predicate has different missing-value semantics")]
    PredicateMissingValueSemantics,
    #[error("the automatic playlist uses an unsafe rating not-equal predicate")]
    RatingNotEqual,
    #[error("source ratings cannot be converted without changing query membership")]
    RatingGrid,
}

/// Translate the exact supported Rhythmbox automatic-playlist subset.
///
/// `source_ratings` must contain the ratings from the complete parsed source
/// snapshot, not only currently matched tracks. Numeric rating translation is
/// accepted only when every positive source rating lies on the same 0.05-star
/// grid used by the migration's `round(value * 20)` conversion.
pub(super) fn translate_automatic_playlist(
    source: &RhythmboxAutomaticPlaylist,
    source_ratings: &[RhythmboxRating],
) -> Result<SmartRules, RhythmboxSmartPlaylistUnsupported> {
    validate_playlist_attributes(&source.attributes)?;
    let (match_mode, predicates) = query_predicates(source)?;

    let mut translated = Vec::with_capacity(predicates.len());
    let mut uses_numeric_rating = false;
    for predicate in predicates {
        let result = translate_predicate(predicate)?;
        uses_numeric_rating |= result.uses_numeric_rating;
        translated.push(result);
    }

    if uses_numeric_rating && !ratings_use_exact_grid(source_ratings) {
        return Err(RhythmboxSmartPlaylistUnsupported::RatingGrid);
    }

    let guarantees_rated = match &match_mode {
        MatchMode::All => translated.iter().any(|rule| rule.implies_rated),
        MatchMode::Any => translated.iter().all(|rule| rule.implies_rated),
    };
    if translated.iter().any(|rule| rule.requires_rated) && !guarantees_rated {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateMissingValueSemantics);
    }

    Ok(SmartRules {
        match_mode,
        rules: translated.into_iter().map(|rule| rule.rule).collect(),
        limit: None,
        sort_order: Vec::new(),
    })
}

fn validate_playlist_attributes(
    attributes: &[RhythmboxXmlAttribute],
) -> Result<(), RhythmboxSmartPlaylistUnsupported> {
    let mut limit_count = None;
    let mut saw_sort_key = false;
    let mut saw_sort_direction = false;
    let mut saw_show_browser = false;
    let mut saw_browser_position = false;
    let mut saw_search_type = false;
    for attribute in attributes {
        match attribute.name.as_str() {
            // Rhythmbox writes these common source-presentation settings for
            // every settings-backed playlist. They select only browser UI
            // state and never participate in automatic-query membership.
            "show-browser"
                if !saw_show_browser && is_membership_inert_playlist_attribute(attribute) =>
            {
                saw_show_browser = true;
            }
            "browser-position"
                if !saw_browser_position && is_membership_inert_playlist_attribute(attribute) =>
            {
                saw_browser_position = true;
            }
            "search-type"
                if !saw_search_type && is_membership_inert_playlist_attribute(attribute) =>
            {
                saw_search_type = true;
            }
            "limit-count" if limit_count.is_none() => {
                limit_count = Some(parse_item_limit(&attribute.value)?);
            }
            // Rhythmbox gives count precedence over these attributes, but
            // accepting an inert second limit would silently discard source
            // configuration. The exact subset rejects their mere presence.
            "limit-size" | "limit-time" | "limit" => {
                return Err(RhythmboxSmartPlaylistUnsupported::LimitKind);
            }
            // Rhythmbox numeric comparators fall back to source-location
            // ordering when primary values tie. Tributary's presentation and
            // limit sorts retain a different order or use track ID/history.
            // Until SmartRules can express that location tie-break, even a
            // known numeric source key cannot be translated exactly.
            "sort-key" if !saw_sort_key => saw_sort_key = true,
            "sort-direction" if !saw_sort_direction => saw_sort_direction = true,
            "limit-count" => {
                return Err(RhythmboxSmartPlaylistUnsupported::PlaylistAttribute);
            }
            "sort-key" | "sort-direction" => {
                return Err(RhythmboxSmartPlaylistUnsupported::PlaylistAttribute);
            }
            _ => return Err(RhythmboxSmartPlaylistUnsupported::PlaylistAttribute),
        }
    }
    if limit_count.is_some_and(|value| value != 0) {
        return Err(RhythmboxSmartPlaylistUnsupported::LimitOrdering);
    }
    if saw_sort_key {
        return Err(RhythmboxSmartPlaylistUnsupported::SortTieBreak);
    }
    if saw_sort_direction {
        return Err(RhythmboxSmartPlaylistUnsupported::SortConfiguration);
    }
    Ok(())
}

fn parse_item_limit(value: &str) -> Result<u32, RhythmboxSmartPlaylistUnsupported> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RhythmboxSmartPlaylistUnsupported::LimitValue);
    }
    value
        .parse::<u32>()
        .map_err(|_| RhythmboxSmartPlaylistUnsupported::LimitValue)
}

fn query_predicates(
    source: &RhythmboxAutomaticPlaylist,
) -> Result<(MatchMode, Vec<&RhythmboxAutomaticNode>), RhythmboxSmartPlaylistUnsupported> {
    let [outer] = source.query.as_slice() else {
        return Err(RhythmboxSmartPlaylistUnsupported::QueryRoot);
    };
    if outer.element != "conjunction" || !outer.attributes.is_empty() || !outer.text.is_empty() {
        return Err(RhythmboxSmartPlaylistUnsupported::QueryRoot);
    }
    let [song_guard, subquery] = outer.children.as_slice() else {
        return Err(RhythmboxSmartPlaylistUnsupported::OuterShape);
    };
    validate_song_guard(song_guard)?;
    if subquery.element != "subquery"
        || !subquery.attributes.is_empty()
        || !subquery.text.is_empty()
    {
        return Err(RhythmboxSmartPlaylistUnsupported::SubqueryShape);
    }
    let [inner] = subquery.children.as_slice() else {
        return Err(RhythmboxSmartPlaylistUnsupported::SubqueryShape);
    };
    if inner.element != "conjunction" || !inner.attributes.is_empty() || !inner.text.is_empty() {
        return Err(RhythmboxSmartPlaylistUnsupported::SubqueryShape);
    }
    split_flat_boolean(&inner.children)
}

fn validate_song_guard(
    node: &RhythmboxAutomaticNode,
) -> Result<(), RhythmboxSmartPlaylistUnsupported> {
    if node.element != "equals"
        || node.text != "song"
        || !node.children.is_empty()
        || !has_exact_property(&node.attributes, "type")
    {
        return Err(RhythmboxSmartPlaylistUnsupported::SongTypeGuard);
    }
    Ok(())
}

fn split_flat_boolean(
    children: &[RhythmboxAutomaticNode],
) -> Result<(MatchMode, Vec<&RhythmboxAutomaticNode>), RhythmboxSmartPlaylistUnsupported> {
    if children.is_empty() {
        return Err(RhythmboxSmartPlaylistUnsupported::EmptyQuery);
    }

    let separator_count = children
        .iter()
        .filter(|node| node.element == "disjunction")
        .count();
    if separator_count == 0 {
        return Ok((MatchMode::All, children.iter().collect()));
    }
    if children.len().is_multiple_of(2) || separator_count != children.len() / 2 {
        return Err(RhythmboxSmartPlaylistUnsupported::BooleanShape);
    }

    let mut predicates = Vec::with_capacity(separator_count + 1);
    for (index, child) in children.iter().enumerate() {
        if index % 2 == 0 {
            if child.element == "disjunction" {
                return Err(RhythmboxSmartPlaylistUnsupported::BooleanShape);
            }
            predicates.push(child);
        } else if child.element != "disjunction" {
            return Err(RhythmboxSmartPlaylistUnsupported::BooleanShape);
        } else if !child.attributes.is_empty()
            || !child.text.is_empty()
            || !child.children.is_empty()
        {
            return Err(RhythmboxSmartPlaylistUnsupported::BooleanSeparator);
        }
    }
    Ok((MatchMode::Any, predicates))
}

struct TranslatedPredicate {
    rule: SmartRule,
    implies_rated: bool,
    requires_rated: bool,
    uses_numeric_rating: bool,
}

fn translate_predicate(
    source: &RhythmboxAutomaticNode,
) -> Result<TranslatedPredicate, RhythmboxSmartPlaylistUnsupported> {
    if matches!(
        source.element.as_str(),
        "conjunction" | "disjunction" | "subquery"
    ) {
        return Err(RhythmboxSmartPlaylistUnsupported::BooleanShape);
    }
    if !source.children.is_empty() || source.text.trim() != source.text {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateShape);
    }
    let property = exact_property(&source.attributes)?;
    match property {
        "play-count" => translate_play_count(source.element.as_str(), source.text.as_str()),
        "rating" => translate_rating(source.element.as_str(), source.text.as_str()),
        // Text folding and every optional numeric/date field differ between
        // the two engines. They remain closed until an exact representation
        // (including missing-value behavior) exists.
        _ => Err(RhythmboxSmartPlaylistUnsupported::PredicateProperty),
    }
}

fn exact_property(
    attributes: &[RhythmboxXmlAttribute],
) -> Result<&str, RhythmboxSmartPlaylistUnsupported> {
    let [attribute] = attributes else {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateShape);
    };
    if attribute.name != "prop" || attribute.value.is_empty() {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateShape);
    }
    Ok(attribute.value.as_str())
}

fn has_exact_property(attributes: &[RhythmboxXmlAttribute], expected: &str) -> bool {
    matches!(attributes, [attribute] if attribute.name == "prop" && attribute.value == expected)
}

fn translate_play_count(
    operator: &str,
    value: &str,
) -> Result<TranslatedPredicate, RhythmboxSmartPlaylistUnsupported> {
    let value = parse_play_count(value)?;
    let (operator, value) = match operator {
        "equals" => (RuleOperator::Is, i64::from(value)),
        "not-equal" => (RuleOperator::IsNot, i64::from(value)),
        // Rhythmbox's historical names are misleading: `greater` and
        // `less` are inclusive ("at least" and "at most"). Tributary's
        // numeric operators are strict, so shift the integer threshold.
        "greater" => (RuleOperator::GreaterThan, i64::from(value) - 1),
        "less" => (RuleOperator::LessThan, i64::from(value) + 1),
        _ => return Err(RhythmboxSmartPlaylistUnsupported::PredicateOperator),
    };
    Ok(TranslatedPredicate {
        rule: SmartRule {
            field: RuleField::PlayCount,
            operator,
            value: RuleValue::Number(value),
        },
        implies_rated: false,
        requires_rated: false,
        uses_numeric_rating: false,
    })
}

fn parse_play_count(value: &str) -> Result<i32, RhythmboxSmartPlaylistUnsupported> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateValue);
    }
    let value = value
        .parse::<u64>()
        .map_err(|_| RhythmboxSmartPlaylistUnsupported::PredicateValue)?;
    i32::try_from(value).map_err(|_| RhythmboxSmartPlaylistUnsupported::PredicateRange)
}

fn translate_rating(
    operator: &str,
    value: &str,
) -> Result<TranslatedPredicate, RhythmboxSmartPlaylistUnsupported> {
    let native = parse_rating(value)?;
    if native == 0.0 {
        let operator = match operator {
            "equals" => RuleOperator::IsUnrated,
            "not-equal" => RuleOperator::IsRated,
            "greater" | "less" => {
                return Err(RhythmboxSmartPlaylistUnsupported::PredicateMissingValueSemantics);
            }
            _ => return Err(RhythmboxSmartPlaylistUnsupported::PredicateOperator),
        };
        return Ok(TranslatedPredicate {
            implies_rated: matches!(&operator, RuleOperator::IsRated),
            rule: SmartRule {
                field: RuleField::Rating,
                operator,
                // Presence operators require the canonical inert placeholder.
                value: RuleValue::Number(1),
            },
            requires_rated: false,
            uses_numeric_rating: false,
        });
    }

    let canonical = exact_canonical_rating(native)?;
    let (operator, threshold, requires_rated) = match operator {
        "equals" => (RuleOperator::Is, canonical, false),
        // Rhythmbox's zero-valued missing rating satisfies positive
        // not-equal, while Tributary deliberately makes a missing rating
        // satisfy no numeric predicate. Reject rather than invent an OR.
        "not-equal" => return Err(RhythmboxSmartPlaylistUnsupported::RatingNotEqual),
        // The target evaluator deliberately rejects non-canonical numeric
        // operands outside 1..=100. Collapse the two inclusive edge cases to
        // the exact presence predicate instead of emitting `> 0` or `< 101`,
        // both of which would silently evaluate false.
        "greater" if canonical == 1 => (RuleOperator::IsRated, 1, false),
        "greater" => (RuleOperator::GreaterThan, canonical - 1, false),
        "less" if canonical == 100 => (RuleOperator::IsRated, 1, true),
        "less" => (RuleOperator::LessThan, canonical + 1, true),
        _ => return Err(RhythmboxSmartPlaylistUnsupported::PredicateOperator),
    };
    Ok(TranslatedPredicate {
        rule: SmartRule {
            field: RuleField::Rating,
            operator,
            value: RuleValue::Number(threshold),
        },
        // Equality and an inclusive positive lower bound both exclude the
        // source's zero/unrated value.
        implies_rated: !requires_rated,
        requires_rated,
        uses_numeric_rating: true,
    })
}

fn parse_rating(value: &str) -> Result<f64, RhythmboxSmartPlaylistUnsupported> {
    if value.is_empty() || value.trim() != value {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateValue);
    }
    let value = value
        .parse::<f64>()
        .map_err(|_| RhythmboxSmartPlaylistUnsupported::PredicateValue)?;
    if !value.is_finite() || !(0.0..=5.0).contains(&value) {
        return Err(RhythmboxSmartPlaylistUnsupported::PredicateRange);
    }
    Ok(value)
}

fn exact_canonical_rating(value: f64) -> Result<i64, RhythmboxSmartPlaylistUnsupported> {
    // Build the canonical binary values from exact decimal spellings once.
    // Some Windows release targets retain excess precision for arithmetic
    // expressions, so deriving the grid with floating-point division can make
    // a non-grid source value compare equal. Rust's decimal parser produces a
    // fully materialized IEEE-754 value without target floating-point math.
    static GRID: OnceLock<[u64; 100]> = OnceLock::new();
    let grid = GRID.get_or_init(|| {
        std::array::from_fn(|index| {
            let twentieths = index + 1;
            let whole = twentieths / 20;
            let hundredths = (twentieths % 20) * 5;
            format!("{whole}.{hundredths:02}")
                .parse::<f64>()
                .expect("canonical Rhythmbox rating literal")
                .to_bits()
        })
    });
    grid.iter()
        .position(|bits| *bits == value.to_bits())
        .and_then(|index| i64::try_from(index + 1).ok())
        .ok_or(RhythmboxSmartPlaylistUnsupported::RatingGrid)
}

fn ratings_use_exact_grid(ratings: &[RhythmboxRating]) -> bool {
    ratings.iter().all(|rating| {
        let value = rating.value();
        value == 0.0 || exact_canonical_rating(value).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::db::entities::track;
    use crate::local::rhythmbox_import::{
        parse_rhythmbox_documents, RhythmboxImportLimits, RhythmboxPlaylistKind,
    };

    fn attribute(name: &str, value: &str) -> RhythmboxXmlAttribute {
        RhythmboxXmlAttribute {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    fn node(element: &str, property: &str, text: &str) -> RhythmboxAutomaticNode {
        RhythmboxAutomaticNode {
            element: element.to_owned(),
            attributes: vec![attribute("prop", property)],
            text: text.to_owned(),
            children: Vec::new(),
        }
    }

    fn separator() -> RhythmboxAutomaticNode {
        RhythmboxAutomaticNode {
            element: "disjunction".to_owned(),
            attributes: Vec::new(),
            text: String::new(),
            children: Vec::new(),
        }
    }

    fn automatic(
        predicates: Vec<RhythmboxAutomaticNode>,
        attributes: Vec<RhythmboxXmlAttribute>,
    ) -> RhythmboxAutomaticPlaylist {
        RhythmboxAutomaticPlaylist {
            attributes,
            query: vec![RhythmboxAutomaticNode {
                element: "conjunction".to_owned(),
                attributes: Vec::new(),
                text: String::new(),
                children: vec![
                    node("equals", "type", "song"),
                    RhythmboxAutomaticNode {
                        element: "subquery".to_owned(),
                        attributes: Vec::new(),
                        text: String::new(),
                        children: vec![RhythmboxAutomaticNode {
                            element: "conjunction".to_owned(),
                            attributes: Vec::new(),
                            text: String::new(),
                            children: predicates,
                        }],
                    },
                ],
            }],
        }
    }

    fn parsed_ratings(values: &[&str]) -> Vec<RhythmboxRating> {
        let mut xml = "<rhythmdb version=\"2.0\">".to_owned();
        for (index, value) in values.iter().enumerate() {
            write!(
                xml,
                "<entry type=\"song\"><location>file:///rating-{index}</location><rating>{value}</rating></entry>"
            )
            .unwrap();
        }
        xml.push_str("</rhythmdb>");
        parse_rhythmbox_documents(xml.as_bytes(), None, RhythmboxImportLimits::default())
            .unwrap()
            .tracks
            .into_iter()
            .filter_map(|track| track.rating)
            .collect()
    }

    fn translated_rule(
        operator: &str,
        property: &str,
        value: &str,
    ) -> Result<SmartRule, RhythmboxSmartPlaylistUnsupported> {
        let rules = translate_automatic_playlist(
            &automatic(vec![node(operator, property, value)], Vec::new()),
            &[],
        )?;
        Ok(rules.rules.into_iter().next().unwrap())
    }

    fn number(rule: &SmartRule) -> i64 {
        let RuleValue::Number(value) = &rule.value else {
            panic!("expected numeric rule value");
        };
        *value
    }

    fn local_track(id: &str, rating: Option<i32>) -> track::Model {
        track::Model {
            id: id.to_owned(),
            file_path: format!("/{id}.flac"),
            title: id.to_owned(),
            artist_name: String::new(),
            album_artist_name: None,
            album_title: String::new(),
            genre: None,
            composer: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: 0,
            last_played_at_ms: None,
            rating,
            date_added: "2026-07-20T00:00:00Z".to_owned(),
            date_modified: "2026-07-20T00:00:00Z".to_owned(),
            file_size_bytes: None,
        }
    }

    #[test]
    fn translates_all_play_count_operators_with_inclusive_boundary_shifts() {
        let cases = [
            ("equals", 0, RuleOperator::Is, 0),
            ("not-equal", 7, RuleOperator::IsNot, 7),
            ("greater", 0, RuleOperator::GreaterThan, -1),
            (
                "greater",
                i32::MAX,
                RuleOperator::GreaterThan,
                i64::from(i32::MAX) - 1,
            ),
            ("less", 0, RuleOperator::LessThan, 1),
            (
                "less",
                i32::MAX,
                RuleOperator::LessThan,
                i64::from(i32::MAX) + 1,
            ),
        ];
        for (source_operator, source_value, expected_operator, expected_value) in cases {
            let rule =
                translated_rule(source_operator, "play-count", &source_value.to_string()).unwrap();
            assert_eq!(rule.field, RuleField::PlayCount);
            assert_eq!(
                std::mem::discriminant(&rule.operator),
                std::mem::discriminant(&expected_operator)
            );
            assert_eq!(number(&rule), expected_value);
        }
    }

    #[test]
    fn translates_rating_presence_and_positive_grid_values() {
        let unrated = translated_rule("equals", "rating", "0").unwrap();
        assert!(matches!(unrated.operator, RuleOperator::IsUnrated));
        assert_eq!(number(&unrated), 1);

        let rated = translated_rule("not-equal", "rating", "0.0").unwrap();
        assert!(matches!(rated.operator, RuleOperator::IsRated));

        let equal = translated_rule("equals", "rating", "3.65").unwrap();
        assert!(matches!(equal.operator, RuleOperator::Is));
        assert_eq!(number(&equal), 73);

        let lower = translated_rule("greater", "rating", "0.05").unwrap();
        assert!(matches!(lower.operator, RuleOperator::IsRated));
        assert_eq!(number(&lower), 1);
    }

    #[test]
    fn positive_rating_upper_bound_requires_an_all_mode_rated_guard() {
        let unsupported = automatic(vec![node("less", "rating", "4")], Vec::new());
        assert_eq!(
            translate_automatic_playlist(&unsupported, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateMissingValueSemantics
        );

        let supported = automatic(
            vec![
                node("not-equal", "rating", "0"),
                node("less", "rating", "4"),
            ],
            Vec::new(),
        );
        let translated = translate_automatic_playlist(&supported, &[]).unwrap();
        assert_eq!(translated.match_mode, MatchMode::All);
        assert!(matches!(
            translated.rules[1].operator,
            RuleOperator::LessThan
        ));
        assert_eq!(number(&translated.rules[1]), 81);

        let full_range = automatic(
            vec![
                node("not-equal", "rating", "0"),
                node("less", "rating", "5"),
            ],
            Vec::new(),
        );
        let translated = translate_automatic_playlist(&full_range, &[]).unwrap();
        assert!(matches!(
            translated.rules[1].operator,
            RuleOperator::IsRated
        ));
        assert_eq!(number(&translated.rules[1]), 1);
    }

    #[test]
    fn translated_rating_boundaries_evaluate_with_canonical_operands() {
        let tracks = [
            local_track("unrated", None),
            local_track("minimum", Some(1)),
            local_track("maximum", Some(100)),
        ];
        let lower = translate_automatic_playlist(
            &automatic(vec![node("greater", "rating", "0.05")], Vec::new()),
            &[],
        )
        .unwrap();
        assert_eq!(
            super::super::smart_rules::evaluate(&lower, &tracks)
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["minimum", "maximum"]
        );

        let full_range = translate_automatic_playlist(
            &automatic(
                vec![
                    node("not-equal", "rating", "0"),
                    node("less", "rating", "5"),
                ],
                Vec::new(),
            ),
            &[],
        )
        .unwrap();
        assert_eq!(
            super::super::smart_rules::evaluate(&full_range, &tracks)
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["minimum", "maximum"]
        );
    }

    #[test]
    fn translates_flat_disjunction_only_when_every_gap_is_a_separator() {
        let source = automatic(
            vec![
                node("equals", "play-count", "0"),
                separator(),
                node("greater", "rating", "4"),
                separator(),
                node("equals", "rating", "0"),
            ],
            Vec::new(),
        );
        let translated = translate_automatic_playlist(&source, &[]).unwrap();
        assert_eq!(translated.match_mode, MatchMode::Any);
        assert_eq!(translated.rules.len(), 3);

        let mixed = automatic(
            vec![
                node("equals", "play-count", "0"),
                node("equals", "play-count", "1"),
                separator(),
                node("equals", "play-count", "2"),
            ],
            Vec::new(),
        );
        assert_eq!(
            translate_automatic_playlist(&mixed, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::BooleanShape
        );
    }

    #[test]
    fn rejects_active_count_limits_and_every_explicit_source_sort() {
        for wire_direction in ["0", "1"] {
            let source = automatic(
                vec![node("greater", "play-count", "0")],
                vec![
                    attribute("limit-count", "25"),
                    attribute("sort-direction", wire_direction),
                    attribute("sort-key", "PlayCount"),
                ],
            );
            assert_eq!(
                translate_automatic_playlist(&source, &[]).unwrap_err(),
                RhythmboxSmartPlaylistUnsupported::LimitOrdering
            );
        }

        for key in ["PlayCount", "Rating"] {
            for direction in ["0", "1"] {
                let source = automatic(
                    vec![node("not-equal", "rating", "0")],
                    vec![
                        attribute("sort-direction", direction),
                        attribute("sort-key", key),
                    ],
                );
                assert_eq!(
                    translate_automatic_playlist(&source, &[]).unwrap_err(),
                    RhythmboxSmartPlaylistUnsupported::SortTieBreak
                );
            }
        }
    }

    #[test]
    fn accepts_only_a_zero_item_limit_as_a_semantically_inert_attribute() {
        let source = automatic(
            vec![node("equals", "play-count", "1")],
            vec![attribute("limit-count", "0")],
        );
        let translated = translate_automatic_playlist(&source, &[]).unwrap();
        assert!(translated.limit.is_none());
        assert!(translated.sort_order.is_empty());
    }

    #[test]
    fn zero_limit_does_not_make_an_explicit_sort_inert() {
        let source = automatic(
            vec![node("equals", "play-count", "1")],
            vec![
                attribute("limit-count", "0"),
                attribute("sort-key", "PlayCount"),
            ],
        );
        assert_eq!(
            translate_automatic_playlist(&source, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::SortTieBreak
        );
    }

    #[test]
    fn rejects_nested_empty_and_malformed_outer_shapes() {
        let empty = automatic(Vec::new(), Vec::new());
        assert_eq!(
            translate_automatic_playlist(&empty, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::EmptyQuery
        );

        let nested = automatic(
            vec![RhythmboxAutomaticNode {
                element: "subquery".to_owned(),
                attributes: Vec::new(),
                text: String::new(),
                children: vec![node("equals", "play-count", "1")],
            }],
            Vec::new(),
        );
        assert_eq!(
            translate_automatic_playlist(&nested, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::BooleanShape
        );

        let mut invalid_separator = separator();
        invalid_separator.text = "not-empty".to_owned();
        let invalid_separator = automatic(
            vec![
                node("equals", "play-count", "0"),
                invalid_separator,
                node("equals", "play-count", "1"),
            ],
            Vec::new(),
        );
        assert_eq!(
            translate_automatic_playlist(&invalid_separator, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::BooleanSeparator
        );

        let mut wrong_guard = automatic(vec![node("equals", "play-count", "1")], Vec::new());
        wrong_guard.query[0].children[0].text = "podcast".to_owned();
        assert_eq!(
            translate_automatic_playlist(&wrong_guard, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::SongTypeGuard
        );

        let mut extra_outer = automatic(vec![node("equals", "play-count", "1")], Vec::new());
        extra_outer.query[0]
            .children
            .push(node("equals", "play-count", "2"));
        assert_eq!(
            translate_automatic_playlist(&extra_outer, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::OuterShape
        );
    }

    #[test]
    fn rejects_unsupported_properties_operators_and_unsafe_rating_forms() {
        assert_eq!(
            translated_rule("equals", "genre", "Rock").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateProperty
        );
        assert_eq!(
            translated_rule("like", "play-count", "1").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateOperator
        );
        assert_eq!(
            translated_rule("not-equal", "rating", "4").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::RatingNotEqual
        );
        assert_eq!(
            translated_rule("greater", "rating", "0").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateMissingValueSemantics
        );
        assert_eq!(
            translated_rule("equals", "rating", "4.01").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::RatingGrid
        );
    }

    #[test]
    fn rejects_invalid_values_limits_sorts_and_optional_numeric_fields() {
        assert_eq!(
            translated_rule("equals", "play-count", "-1").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateValue
        );
        assert_eq!(
            translated_rule("equals", "play-count", "2147483648").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateRange
        );
        assert_eq!(
            translated_rule("equals", "duration", "60").unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::PredicateProperty
        );

        for (attributes, expected) in [
            (
                vec![attribute("limit-time", "60")],
                RhythmboxSmartPlaylistUnsupported::LimitKind,
            ),
            (
                vec![attribute("limit-size", "1024")],
                RhythmboxSmartPlaylistUnsupported::LimitKind,
            ),
            (
                vec![attribute("limit-count", "10")],
                RhythmboxSmartPlaylistUnsupported::LimitOrdering,
            ),
            (
                vec![attribute("limit-count", "-1")],
                RhythmboxSmartPlaylistUnsupported::LimitValue,
            ),
            (
                vec![attribute("sort-key", "Title")],
                RhythmboxSmartPlaylistUnsupported::SortTieBreak,
            ),
            (
                vec![attribute("sort-direction", "1")],
                RhythmboxSmartPlaylistUnsupported::SortConfiguration,
            ),
            (
                vec![
                    attribute("sort-key", "PlayCount"),
                    attribute("sort-direction", "2"),
                ],
                RhythmboxSmartPlaylistUnsupported::SortTieBreak,
            ),
        ] {
            let source = automatic(vec![node("equals", "play-count", "1")], attributes);
            assert_eq!(
                translate_automatic_playlist(&source, &[]).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn source_rating_grid_is_checked_for_numeric_rating_queries() {
        for canonical in 1_i64..=100 {
            let whole = canonical / 20;
            let hundredths = (canonical % 20) * 5;
            let native = format!("{whole}.{hundredths:02}").parse::<f64>().unwrap();
            assert_eq!(exact_canonical_rating(native), Ok(canonical));
            assert_eq!(
                exact_canonical_rating(f64::from_bits(native.to_bits() - 1)).unwrap_err(),
                RhythmboxSmartPlaylistUnsupported::RatingGrid
            );
            assert_eq!(
                exact_canonical_rating(f64::from_bits(native.to_bits() + 1)).unwrap_err(),
                RhythmboxSmartPlaylistUnsupported::RatingGrid
            );
        }

        let source = automatic(vec![node("equals", "rating", "4")], Vec::new());
        assert!(
            translate_automatic_playlist(&source, &parsed_ratings(&["0", "3.95", "5"])).is_ok()
        );
        assert_eq!(
            translate_automatic_playlist(&source, &parsed_ratings(&["4.01"])).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::RatingGrid
        );
        assert_eq!(
            translate_automatic_playlist(&source, &parsed_ratings(&["0.30000000000000004"]))
                .unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::RatingGrid
        );

        // Presence-only rules do not round any source rating and therefore do
        // not need the numeric grid restriction.
        let presence = automatic(vec![node("not-equal", "rating", "0")], Vec::new());
        assert!(translate_automatic_playlist(&presence, &parsed_ratings(&["4.01"])).is_ok());
    }

    #[test]
    fn parser_output_for_canonical_query_creator_shape_translates() {
        let playlists = br#"
          <rhythmdb-playlists>
            <playlist name="Top" type="automatic">
              <conjunction>
                <equals prop="type">song</equals>
                <subquery><conjunction>
                  <greater prop="play-count">10</greater>
                  <disjunction/>
                  <equals prop="rating">5</equals>
                </conjunction></subquery>
              </conjunction>
            </playlist>
          </rhythmdb-playlists>
        "#;
        let parsed = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(playlists),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let RhythmboxPlaylistKind::Automatic(source) = &parsed.playlists[0].kind else {
            panic!("expected automatic playlist");
        };
        let translated = translate_automatic_playlist(source, &[]).unwrap();
        assert_eq!(translated.match_mode, MatchMode::Any);
        assert_eq!(translated.rules.len(), 2);
        assert!(translated.limit.is_none());
        assert!(translated.sort_order.is_empty());
    }

    #[test]
    fn parser_output_with_rhythmbox_source_presentation_attributes_translates() {
        // rb_playlist_source_save_to_xml writes these three settings before
        // the automatic-playlist implementation serializes its query.
        let playlists = br#"
          <rhythmdb-playlists>
            <playlist name="Top" type="automatic" show-browser="true"
                      browser-position="180" search-type="search-match">
              <conjunction>
                <equals prop="type">song</equals>
                <subquery><conjunction>
                  <greater prop="play-count">10</greater>
                </conjunction></subquery>
              </conjunction>
            </playlist>
          </rhythmdb-playlists>
        "#;
        let parsed = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(playlists),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let RhythmboxPlaylistKind::Automatic(source) = &parsed.playlists[0].kind else {
            panic!("expected automatic playlist");
        };
        let translated = translate_automatic_playlist(source, &[]).unwrap();
        assert_eq!(translated.match_mode, MatchMode::All);
        assert_eq!(translated.rules.len(), 1);
    }

    #[test]
    fn malformed_or_duplicate_presentation_attributes_remain_rejected() {
        for attributes in [
            vec![attribute("show-browser", "1")],
            vec![attribute("browser-position", "+180")],
            vec![
                attribute("search-type", "search-match"),
                attribute("search-type", "search-title"),
            ],
        ] {
            let source = automatic(vec![node("equals", "play-count", "1")], attributes);
            assert_eq!(
                translate_automatic_playlist(&source, &[]).unwrap_err(),
                RhythmboxSmartPlaylistUnsupported::PlaylistAttribute
            );
        }
    }

    #[test]
    fn parser_output_with_active_limit_and_sort_is_rejected_losslessly() {
        let playlists = br#"
          <rhythmdb-playlists>
            <playlist name="Top" type="automatic" limit-count="25"
                      sort-key="PlayCount" sort-direction="1">
              <conjunction>
                <equals prop="type">song</equals>
                <subquery><conjunction>
                  <greater prop="play-count">10</greater>
                </conjunction></subquery>
              </conjunction>
            </playlist>
          </rhythmdb-playlists>
        "#;
        let parsed = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"/>",
            Some(playlists),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let RhythmboxPlaylistKind::Automatic(source) = &parsed.playlists[0].kind else {
            panic!("expected automatic playlist");
        };
        assert_eq!(
            translate_automatic_playlist(source, &[]).unwrap_err(),
            RhythmboxSmartPlaylistUnsupported::LimitOrdering
        );
    }

    #[test]
    fn errors_and_debug_output_do_not_echo_source_content() {
        let secret = "PRIVATE-PLAYLIST-QUERY-VALUE";
        let source = automatic(
            vec![node("equals", "private-field", secret)],
            vec![attribute("private-attribute", secret)],
        );
        let error = translate_automatic_playlist(&source, &[]).unwrap_err();
        let output = format!("{error:?} {error}");
        assert!(!output.contains(secret));
        assert!(!output.contains("private-field"));
        assert!(!output.contains("private-attribute"));
    }
}
