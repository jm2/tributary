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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use super::*;

    /// A catalog entry's text by plural form; a plain string has one form, "".
    type Entry = BTreeMap<String, String>;

    const PLURAL_FORMS: [&str; 4] = ["one", "few", "many", "other"];

    fn flatten(prefix: &str, value: &serde_yaml::Value, out: &mut BTreeMap<String, Entry>) {
        match value {
            serde_yaml::Value::String(text) => {
                out.insert(
                    prefix.to_owned(),
                    Entry::from([(String::new(), text.clone())]),
                );
            }
            serde_yaml::Value::Mapping(map) => {
                let key = |key: &serde_yaml::Value| {
                    key.as_str()
                        .unwrap_or_else(|| panic!("{prefix}: non-string key"))
                        .to_owned()
                };
                if map
                    .keys()
                    .all(|form| PLURAL_FORMS.contains(&key(form).as_str()))
                {
                    let forms = map
                        .iter()
                        .map(|(form, text)| {
                            let text = text.as_str().unwrap_or_else(|| {
                                panic!("{prefix}.{}: plural form is not text", key(form))
                            });
                            (key(form), text.to_owned())
                        })
                        .collect();
                    out.insert(prefix.to_owned(), forms);
                    return;
                }
                for (name, child) in map {
                    let name = key(name);
                    let path = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}.{name}")
                    };
                    flatten(&path, child, out);
                }
            }
            other => panic!("{prefix}: {other:?} is not text"),
        }
    }

    fn locale_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("locales")
    }

    fn catalog(locale: &str) -> BTreeMap<String, Entry> {
        let path = locale_dir().join(format!("{locale}.yml"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let yaml: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let mut entries = BTreeMap::new();
        flatten("", &yaml, &mut entries);
        entries
    }

    fn placeholders(text: &str) -> BTreeSet<&str> {
        text.split("%{")
            .skip(1)
            .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
            .collect()
    }

    /// Every catalog carries exactly the English keys, with the English
    /// placeholders in every form. Plural entries may carry extra forms (Polish
    /// and Russian add `few` and `many`) but must carry each form
    /// [`plural_category`] can select for that language.
    #[test]
    fn every_catalog_matches_the_english_keys_and_placeholders() {
        let mut files: Vec<String> = std::fs::read_dir(locale_dir())
            .expect("read locales")
            .map(|entry| {
                let name = entry.expect("locale entry").file_name();
                let name = name.to_str().expect("UTF-8 locale file name");
                name.strip_suffix(".yml")
                    .unwrap_or_else(|| panic!("unexpected file locales/{name}"))
                    .to_owned()
            })
            .collect();
        files.sort();
        let mut locales: Vec<String> = rust_i18n::available_locales!()
            .iter()
            .map(ToString::to_string)
            .collect();
        locales.sort();
        assert_eq!(files, locales, "every catalog file is a loaded locale");

        let samples = [
            "0", "1", "2", "3", "5", "11", "12", "21", "22", "25", "1.0", "1.5",
        ];
        let english = catalog("en");
        for locale in &locales {
            let translated = catalog(locale);
            let missing: Vec<_> = english
                .keys()
                .filter(|key| !translated.contains_key(*key))
                .collect();
            let extra: Vec<_> = translated
                .keys()
                .filter(|key| !english.contains_key(*key))
                .collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{locale}: missing {missing:?}, extra {extra:?}"
            );

            for (key, forms) in &translated {
                let reference = &english[key];
                let reference = reference.get("other").unwrap_or_else(|| &reference[""]);
                if !forms.contains_key("") {
                    for sample in samples {
                        let form = plural_category(locale, sample);
                        assert!(
                            forms.contains_key(form),
                            "{locale}.{key} lacks the `{form}` form {sample} selects"
                        );
                    }
                }
                for (form, text) in forms {
                    assert!(!text.trim().is_empty(), "{locale}.{key} {form} is empty");
                    assert_eq!(
                        placeholders(text),
                        placeholders(reference),
                        "{locale}.{key} {form} placeholders"
                    );
                }
            }
        }
    }

    /// Source files under `dir`, excluding separate test-module files.
    fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let path = entry.expect("source entry").path();
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("");
            if path.is_dir() {
                source_files(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && stem != "tests"
                && !stem.ends_with("_tests")
            {
                out.push(path);
            }
        }
    }

    /// `source` up to its first inline test module.
    fn production_code(source: &str) -> &str {
        let lines: Vec<&str> = source.split_inclusive('\n').collect();
        let mut offset = 0;
        for (index, line) in lines.iter().enumerate() {
            if line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test,") {
                let item = lines[index + 1..]
                    .iter()
                    .map(|line| line.trim_end())
                    .find(|line| !line.starts_with("#["))
                    .unwrap_or_default();
                let item = item.strip_prefix("pub ").unwrap_or(item);
                if item.starts_with("mod ") && item.ends_with('{') {
                    return &source[..offset];
                }
            }
            offset += line.len();
        }
        source
    }

    /// Every English key is named by a string literal in production code, so
    /// the catalogs carry no copy that nothing displays. A plural entry is
    /// named by its base key, which [`plural_key`] completes.
    #[test]
    fn every_english_key_is_used_by_production_code() {
        let mut files = Vec::new();
        source_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut code = String::new();
        for file in files {
            let source = std::fs::read_to_string(&file)
                .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
            code.push_str(production_code(&source));
        }
        let unused: Vec<_> = catalog("en")
            .into_keys()
            .filter(|key| !code.contains(&format!("\"{key}\"")))
            .collect();
        assert!(unused.is_empty(), "catalog keys no code uses: {unused:?}");
    }

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
