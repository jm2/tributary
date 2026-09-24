//! Plural selection and decimal formatting for catalog strings.
//!
//! `rust-i18n` interpolates values but neither chooses a plural form nor
//! formats numbers, so both happen here for the shipped catalogs. A plural
//! catalog entry is a map from CLDR category (`one`, `few`, `many`, `other`)
//! to text, and [`plural_key`] names the entry a number selects.

/// The language subtag of a locale such as `pt-BR`.
fn language(locale: &str) -> &str {
    let end = locale
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(locale.len());
    &locale[..end]
}

/// The CLDR plural category of `number` as displayed, e.g. `"3"` or `"1.5"`.
///
/// `number` is the ASCII rendering with `.` as the decimal point, because
/// CLDR rules read the visible digits: "1" and "1.0" can take different
/// forms. Only the categories the shipped catalogs carry are modelled.
pub fn plural_category(locale: &str, number: &str) -> &'static str {
    let (integer, fraction) = number.split_once('.').unwrap_or((number, ""));
    let i: u64 = integer.parse().unwrap_or(0);
    let whole = fraction.is_empty();
    let (tens, hundreds) = (i % 10, i % 100);
    match language(locale) {
        "ja" | "ko" | "zh" => "other",
        "fr" | "pt" if i <= 1 => "one",
        "es" if i == 1 && fraction.bytes().all(|digit| digit == b'0') => "one",
        "fr" | "pt" | "es" => "other",
        "pl" if i == 1 && whole => "one",
        "ru" if whole && tens == 1 && hundreds != 11 => "one",
        "pl" | "ru" if whole && (2..=4).contains(&tens) && !(12..=14).contains(&hundreds) => "few",
        "pl" | "ru" if whole => "many",
        "pl" | "ru" => "other",
        _ if i == 1 && whole => "one",
        _ => "other",
    }
}

/// The catalog key of the `base` plural entry's form for `number`.
pub fn plural_key(base: &str, locale: &str, number: &str) -> String {
    format!("{base}.{}", plural_category(locale, number))
}

/// `number` (ASCII, `.` as the decimal point) with the locale's separator.
pub fn localize_decimal(locale: &str, number: &str) -> String {
    match language(locale) {
        "de" | "es" | "fr" | "it" | "nl" | "pl" | "pt" | "ru" => number.replace('.', ","),
        _ => number.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_numbers_follow_each_languages_cldr_rule() {
        for (locale, number, category) in [
            ("en", "1", "one"),
            ("en", "0", "other"),
            ("en", "21", "other"),
            ("de", "1", "one"),
            ("fr", "0", "one"),
            ("fr", "2", "other"),
            ("pt-BR", "1", "one"),
            ("pt-BR", "0", "one"),
            ("es", "1", "one"),
            ("ja", "1", "other"),
            ("zh-CN", "2", "other"),
            ("pl", "1", "one"),
            ("pl", "2", "few"),
            ("pl", "5", "many"),
            ("pl", "12", "many"),
            ("pl", "21", "many"),
            ("pl", "22", "few"),
            ("ru", "1", "one"),
            ("ru", "21", "one"),
            ("ru", "11", "many"),
            ("ru", "3", "few"),
            ("ru", "5", "many"),
        ] {
            assert_eq!(
                plural_category(locale, number),
                category,
                "{locale} {number}"
            );
        }
    }

    #[test]
    fn visible_decimals_change_the_category() {
        for (locale, number, category) in [
            ("en", "1.0", "other"),
            ("de", "1.5", "other"),
            ("es", "1.0", "one"),
            ("es", "1.5", "other"),
            ("fr", "1.5", "one"),
            ("pt-BR", "1.5", "one"),
            ("fr", "2.5", "other"),
            ("pl", "1.5", "other"),
            ("ru", "2.0", "other"),
        ] {
            assert_eq!(
                plural_category(locale, number),
                category,
                "{locale} {number}"
            );
        }
    }

    #[test]
    fn decimal_separator_follows_the_locale() {
        assert_eq!(localize_decimal("en", "3.4"), "3.4");
        assert_eq!(localize_decimal("zh-TW", "3.4"), "3.4");
        assert_eq!(localize_decimal("de", "3.4"), "3,4");
        assert_eq!(localize_decimal("pt-BR", "3.4"), "3,4");
    }
}
