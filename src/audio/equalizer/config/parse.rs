//! Strict parsing of the `equalizer.cfg` grammar: the line scanner,
//! the required-key validation (no per-key defaults), and the
//! contract's coercion and clamp rules applied on read.

use crate::audio::equalizer::{ClipProtection, EqSettings, Preset};

use super::SCHEMA_VERSION;

/// Line-scan accumulator for the config parser. Every key is optional
/// while scanning and *required* at the end: a file that parses but
/// omits any of the fifteen keys is malformed as a whole — there are no
/// per-key defaults for missing keys, because silently filling gaps
/// would combine stale band values with fresh ones (contract:
/// *Persistence*, validation rules).
#[derive(Default)]
struct RawEqConfig {
    schema_version: Option<String>,
    enabled: Option<bool>,
    preset: Option<Preset>,
    preamp_db: Option<f64>,
    bands_db: [Option<f64>; 10],
    clip_protection: Option<ClipProtection>,
}

impl RawEqConfig {
    /// Fold one parsed `key="value"` pair into the accumulator.
    ///
    /// A schema key that is already present is a malformed file, not an
    /// overwrite: the parser is order-insensitive, so no positional
    /// precedence (first or last wins) could be stated without making
    /// the parsed state depend on line order. The error names the
    /// duplicated key for the replacement diagnostic (contract:
    /// *Persistence*, validation rules). Unknown keys stay ignored for
    /// schema forward-compatibility, so their duplicates are ignored
    /// with them.
    fn absorb(&mut self, key: &str, value: &str) -> Result<(), String> {
        if key == "preamp_db" {
            if self.preamp_db.is_some() {
                return Err(key.to_string());
            }
            self.preamp_db = Some(required_gain(value, "preamp_db")?);
            return Ok(());
        }
        if self.absorb_scalar(key, value)? {
            return Ok(());
        }
        self.absorb_band(key, value)
    }

    /// Fold one enum-ish scalar key. Returns `Ok(false)` when the key is
    /// not one of the scalars, leaving it to the band/required-gain
    /// handlers; `Err` names a duplicated key.
    fn absorb_scalar(&mut self, key: &str, value: &str) -> Result<bool, String> {
        match key {
            "schema_version" => {
                Self::require_fresh(key, self.schema_version.is_some())?;
                self.schema_version = Some(value.to_string());
            }
            "enabled" => {
                Self::require_fresh(key, self.enabled.is_some())?;
                self.enabled = Some(value == "true");
            }
            "preset" => {
                Self::require_fresh(key, self.preset.is_some())?;
                self.preset = Some(Preset::from_key(value));
            }
            "clip_protect" => {
                Self::require_fresh(key, self.clip_protection.is_some())?;
                self.clip_protection = Some(ClipProtection::from_key(value));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Reject a second occurrence of a schema key by naming it.
    fn require_fresh(key: &str, already_present: bool) -> Result<(), String> {
        if already_present {
            return Err(key.to_string());
        }
        Ok(())
    }

    /// Fold one `band<N>_db` key. Unknown keys are ignored so a future
    /// minor schema can add keys without discarding user state.
    fn absorb_band(&mut self, key: &str, value: &str) -> Result<(), String> {
        if let Some(band) = band_index(key) {
            if self.bands_db[band].is_some() {
                return Err(key.to_string());
            }
            self.bands_db[band] = Some(parse_gain(value).ok_or_else(|| format!("band{band}_db"))?);
        }
        Ok(())
    }

    /// Finish the scan: require the schema version *and* all fifteen
    /// keys — a file omitting any key is malformed and reported with the
    /// first missing key — then normalize the gains (range clamp, then
    /// the 0.5 dB step snap) and reconcile the preset name with the
    /// normalized values.
    fn into_settings(self) -> Result<EqSettings, String> {
        let schema_version = required(self.schema_version, "schema_version")?;
        if schema_version != SCHEMA_VERSION {
            return Err("schema_version".to_string());
        }
        let settings = EqSettings {
            enabled: required(self.enabled, "enabled")?,
            preset: required(self.preset, "preset")?,
            preamp_db: required(self.preamp_db, "preamp_db")?,
            bands_db: required_bands(self.bands_db)?,
            clip_protection: required(self.clip_protection, "clip_protect")?,
        };
        Ok(reconcile_preset_truth(normalize_gains(settings)))
    }
}

/// Required-key helper for the strict grammar: the missing key names
/// itself in the error — there are no per-key defaults.
fn required<T>(field: Option<T>, key: &str) -> Result<T, String> {
    field.ok_or_else(|| key.to_string())
}

/// Require all ten band gains, reporting the first missing `band<N>_db`
/// key.
fn required_bands(bands_db: [Option<f64>; 10]) -> Result<[f64; 10], String> {
    let mut bands = [0.0; 10];
    for (index, slot) in bands.iter_mut().enumerate() {
        *slot = required(bands_db[index], &format!("band{index}_db"))?;
    }
    Ok(bands)
}

/// Normalize preamp and band gains across the read boundary: clamp into
/// the contract bounds, then snap to the 0.5 dB step grid, ties away
/// from zero. The normalized value is what the runtime materializes and
/// what the next save persists, so no off-grid value survives the read
/// (contract: *Persistence*, validation rules).
fn normalize_gains(mut settings: EqSettings) -> EqSettings {
    settings.preamp_db = EqSettings::normalize_gain_db(settings.preamp_db);
    for gain in &mut settings.bands_db {
        *gain = EqSettings::normalize_gain_db(*gain);
    }
    settings
}

/// Preset-truth reconciliation: after all per-key coercions, a persisted
/// *named* preset must still describe the values the file carries. An
/// exact match between the normalized band vector (and preamp) and the
/// named preset's canonical definition keeps the name; any difference —
/// including a named preset whose preamp was clamped or snapped away
/// from its canonical recommendation, and a value coerced to `Flat`
/// whose vector is not all zeros — moves the persisted preset to
/// `Custom`. This is a name-side transition only: no stored or coerced
/// value is ever altered here, and `Custom` is left untouched.
fn reconcile_preset_truth(mut settings: EqSettings) -> EqSettings {
    if settings.preset == Preset::Custom {
        return settings;
    }
    let canonical_bands = settings.preset.band_gains_db();
    let canonical_preamp = settings.preset.recommended_preamp_db();
    // Exact comparison is the contract's intent: both sides are
    // multiples of 0.5 dB — exact in f64 — and `==` also treats a
    // negative zero from the snap as equal to canonical +0.0. The test
    // module carries the same justification (contract: *Persistence*,
    // preset-truth reconciliation).
    #[allow(clippy::float_cmp)]
    if settings.bands_db != canonical_bands || settings.preamp_db != canonical_preamp {
        settings.preset = Preset::Custom;
    }
    settings
}

/// Parse the strict `key="value"` grammar. Returns `Err(locator)` for a
/// malformed line, a bad schema version, or an unparseable mandatory
/// key. The locator is a key-or-line locator: the offending key when
/// one is parseable, otherwise the failing line number and a failure
/// category — a line the grammar cannot split has no key to report, and
/// the diagnostic must never carry file content.
pub(super) fn parse_equalizer_file(bytes: &[u8]) -> Result<EqSettings, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "schema_version".to_string())?;
    let mut config = RawEqConfig::default();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = parse_line(line) else {
            return Err(line_locator(index + 1, line));
        };
        config.absorb(&key, &value)?;
    }
    config.into_settings()
}

/// The diagnostic locator for a line that failed the line grammar. When
/// a parseable `key=` prefix exists (the same key shape `parse_line`
/// would accept), the key names the failure; anything else is reported
/// as a numbered, categorized line so no hand-edited or corrupted
/// content can leak into the warn diagnostic.
fn line_locator(line_number: usize, line: &str) -> String {
    match line.split_once('=') {
        Some((key, _rest))
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            key.to_string()
        }
        _ => format!("line {line_number}: unparseable line"),
    }
}

fn band_index(key: &str) -> Option<usize> {
    let rest = key.strip_prefix("band")?;
    let index = rest.strip_suffix("_db")?;
    index.parse::<usize>().ok().filter(|i| *i < 10)
}

/// Parse one gain value: finite float, clamped by the caller.
fn parse_gain(value: &str) -> Option<f64> {
    let gain = value.trim().parse::<f64>().ok()?;
    gain.is_finite().then_some(gain)
}

/// `parse_gain` with the required-key error the strict grammar reports.
fn required_gain(value: &str, key: &str) -> Result<f64, String> {
    parse_gain(value).ok_or_else(|| key.to_string())
}

/// Parse a single `key="value"` line with `\"` / `\\` escapes. A bare
/// (unquoted) value or an unknown escape is malformed.
fn parse_line(line: &str) -> Option<(String, String)> {
    let (key, rest) = line.split_once('=')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let value = quoted_value(rest)?;
    Some((key.to_string(), unescape(value)?))
}

/// Strip the mandatory double quotes around a raw value.
fn quoted_value(rest: &str) -> Option<&str> {
    if rest.len() < 2 {
        return None;
    }
    let value = rest.strip_prefix('"')?.strip_suffix('"')?;
    Some(value)
}

/// Decode `\"` and `\\` escapes. Any other escape, an embedded quote,
/// or an embedded newline is malformed.
fn unescape(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            out.push(escaped_char(&mut chars)?);
        } else if is_forbidden_literal(ch) {
            return None;
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// Decode the character following a `\`. Any escape other than `\"` or
/// `\\` is malformed.
fn escaped_char(chars: &mut std::str::Chars<'_>) -> Option<char> {
    match chars.next()? {
        '"' => Some('"'),
        '\\' => Some('\\'),
        _ => None,
    }
}

/// A raw (unescaped) embedded quote or newline is malformed.
fn is_forbidden_literal(ch: char) -> bool {
    ch == '"' || ch == '\n' || ch == '\r'
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests {
    use crate::audio::equalizer::{MAX_GAIN_DB, MIN_GAIN_DB};

    use super::super::render_equalizer_file;
    use super::*;

    #[test]
    fn parse_round_trips_a_written_file() {
        let settings = EqSettings {
            enabled: true,
            preset: Preset::Rock,
            preamp_db: -1.0,
            bands_db: [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            clip_protection: ClipProtection::Soft,
        };
        let parsed = parse_equalizer_file(render_equalizer_file(&settings).as_bytes())
            .expect("round trip parse");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn parser_is_order_insensitive_but_strict_about_quoting() {
        // Line order is free, but the file must still be whole: all
        // fifteen keys are required, so the shuffled fixture carries
        // every one of them (contract: *Persistence*, validation rules).
        let reordered = "\
clip_protect=\"soft\"
band7_db=\"0.0\"
schema_version=\"1\"
band2_db=\"-3.0\"
preamp_db=\"-24.0\"
band9_db=\"+12.0\"
preset=\"jazz\"
band0_db=\"-24.0\"
enabled=\"true\"
band4_db=\"1.5\"
band1_db=\"2.0\"
band6_db=\"0.0\"
band3_db=\"4.5\"
band8_db=\"-1.0\"
band5_db=\"6.0\"
";
        let parsed = parse_equalizer_file(reordered.as_bytes()).expect("reordered parse");
        assert!(parsed.enabled);
        // Preset-truth reconciliation: the file names Jazz but carries
        // values that are not Jazz's canonical vector and preamp, so
        // the persisted name moves to `Custom` (name-side only — the
        // values below are exactly what the file carried).
        assert_eq!(parsed.preset, Preset::Custom);
        assert_eq!(parsed.preamp_db, -24.0);
        assert_eq!(parsed.bands_db[0], -24.0);
        assert_eq!(parsed.bands_db[1], 2.0);
        assert_eq!(parsed.bands_db[2], -3.0);
        assert_eq!(parsed.bands_db[9], 12.0);
        assert_eq!(parsed.clip_protection, ClipProtection::Soft);

        for malformed in ["preset=flat\n", "preset=flat\r\n", "preset=\"\n"] {
            assert!(
                parse_equalizer_file(malformed.as_bytes()).is_err(),
                "unquoted or empty value must be malformed: {malformed:?}"
            );
        }
    }

    #[test]
    fn out_of_range_gains_clamp_to_the_boundary() {
        let mut content = render_equalizer_file(&EqSettings::default());
        content = content.replace("preamp_db=\"0.0\"", "preamp_db=\"-99.0\"");
        content = content.replace("band0_db=\"0.0\"", "band0_db=\"+99.0\"");
        content = content.replace("band9_db=\"0.0\"", "band9_db=\"13.5\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("clamped parse");
        assert_eq!(parsed.preamp_db, MIN_GAIN_DB);
        assert_eq!(parsed.bands_db[0], MAX_GAIN_DB);
        assert_eq!(parsed.bands_db[9], MAX_GAIN_DB);
        // Other bands remain valid.
        assert_eq!(parsed.bands_db[5], 0.0);
    }

    #[test]
    fn unknown_preset_coerces_to_flat_then_reconciles_to_custom() {
        let content = render_equalizer_file(&EqSettings {
            preset: Preset::Rock,
            bands_db: [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            ..EqSettings::default()
        })
        .replace("preset=\"rock\"", "preset=\"loudness2001\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("coerced parse");
        // The unknown name first coerces to `flat`; the preset-truth
        // reconciliation then moves the persisted name to `custom`
        // because the band vector is not canonical Flat (all zeros).
        // The coerced values themselves are never altered.
        assert_eq!(parsed.preset, Preset::Custom);
        assert_eq!(parsed.bands_db[0], 3.0);
        assert_eq!(parsed.bands_db[1], 2.0);
    }

    /// A file naming a canonical preset whose values are exactly that
    /// preset's canonical definition keeps the name — the contract's
    /// exact-match rule.
    #[test]
    fn canonical_named_preset_keeps_its_name() {
        let settings = EqSettings {
            preset: Preset::Classical,
            preamp_db: Preset::Classical.recommended_preamp_db(),
            bands_db: Preset::Classical.band_gains_db(),
            ..EqSettings::default()
        };
        let parsed =
            parse_equalizer_file(render_equalizer_file(&settings).as_bytes()).expect("parse");
        assert_eq!(parsed.preset, Preset::Classical);
        assert_eq!(parsed, settings);
    }

    /// A named preset whose preamp was moved away from the canonical
    /// recommendation (here by a hand edit of the persisted file)
    /// reconciles to `custom`; the stored preamp is kept, not replaced
    /// by the canonical value.
    #[test]
    fn named_preset_with_off_canonical_preamp_becomes_custom() {
        let content = render_equalizer_file(&EqSettings {
            preset: Preset::Rock,
            preamp_db: -1.0,
            bands_db: Preset::Rock.band_gains_db(),
            ..EqSettings::default()
        })
        .replace("preamp_db=\"-1.0\"", "preamp_db=\"-0.5\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("parse");
        assert_eq!(parsed.preset, Preset::Custom);
        assert_eq!(parsed.preamp_db, -0.5);
        assert_eq!(parsed.bands_db, Preset::Rock.band_gains_db());
    }

    /// Off-grid values inside the range snap to the nearest 0.5 dB step,
    /// ties away from zero; the snapped values are what the runtime
    /// materializes and the next save persists.
    #[test]
    fn off_grid_gains_snap_to_the_half_step_grid() {
        let mut content = render_equalizer_file(&EqSettings::default());
        content = content.replace("preamp_db=\"0.0\"", "preamp_db=\"3.7\"");
        content = content.replace("band0_db=\"0.0\"", "band0_db=\"0.1\"");
        content = content.replace("band1_db=\"0.0\"", "band1_db=\"-0.1\"");
        content = content.replace("band2_db=\"0.0\"", "band2_db=\"-6.25\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("snapped parse");
        assert_eq!(parsed.preamp_db, 3.5);
        assert_eq!(parsed.bands_db[0], 0.0);
        assert_eq!(parsed.bands_db[1], 0.0);
        assert_eq!(parsed.bands_db[2], -6.5);
    }

    /// A file that contains any schema key more than once is malformed
    /// as a whole, and the diagnostic names the duplicated key — no
    /// positional precedence is defined for duplicate lines.
    #[test]
    fn duplicate_schema_keys_are_malformed_and_name_the_key() {
        for duplicated in [
            "enabled",
            "schema_version",
            "preset",
            "clip_protect",
            "preamp_db",
            "band0_db",
            "band9_db",
        ] {
            let lines = render_equalizer_file(&EqSettings::default());
            let duplicated_line = lines
                .lines()
                .find(|line| line.starts_with(&format!("{duplicated}=")))
                .expect("canonical render carries every schema key")
                .to_string();
            let content = format!("{lines}{duplicated_line}\n");
            let error = parse_equalizer_file(content.as_bytes())
                .expect_err("a duplicated schema key must be malformed");
            assert_eq!(error, duplicated, "the diagnostic names the duplicated key");
        }
    }

    /// A malformed line whose `key=` prefix the grammar cannot accept —
    /// or a line with no `=` at all — is reported by line number and
    /// failure category, never by content: the diagnostic must not
    /// disclose arbitrary hand-edited or corrupted file bytes.
    #[test]
    fn unparseable_lines_report_a_non_content_locator() {
        let content = render_equalizer_file(&EqSettings::default())
            .lines()
            .chain(["this line has no equals sign at all"])
            .collect::<Vec<_>>()
            .join("\n");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("no-equals line");
        assert_eq!(error, "line 16: unparseable line");
        assert!(
            !error.contains("this line has"),
            "the locator must not carry the line's content"
        );

        // An invalid key shape (space inside the key) is equally
        // content-free: the grammar cannot accept the prefix as a key.
        let content = render_equalizer_file(&EqSettings::default())
            .lines()
            .chain(["not a key=\"value\""])
            .collect::<Vec<_>>()
            .join("\n");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("bad key shape");
        assert_eq!(error, "line 16: unparseable line");
    }

    #[test]
    fn bad_boolean_and_clip_values_fall_back_to_off_states() {
        let content = render_equalizer_file(&EqSettings {
            enabled: true,
            clip_protection: ClipProtection::Soft,
            ..EqSettings::default()
        })
        .replace("enabled=\"true\"", "enabled=\"maybe\"")
        .replace("clip_protect=\"soft\"", "clip_protect=\"turbo\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("fallback parse");
        assert!(!parsed.enabled);
        assert_eq!(parsed.clip_protection, ClipProtection::Off);
    }

    #[test]
    fn malformed_line_reports_the_bad_key() {
        let content = render_equalizer_file(&EqSettings::default())
            .replace("band3_db=\"0.0\"", "band3_db=\"0.0\" trailing");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("malformed");
        assert_eq!(error, "band3_db");
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let content = render_equalizer_file(&EqSettings::default())
            .replace("schema_version=\"1\"", "schema_version=\"2\"");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("schema");
        assert_eq!(error, "schema_version");
    }

    #[test]
    fn missing_any_of_the_fifteen_keys_is_malformed() {
        // No per-key defaults: a file that parses but omits any of the
        // fifteen keys is malformed as a whole (contract: *Persistence*).
        const ALL_KEYS: [&str; 15] = [
            "schema_version",
            "enabled",
            "preset",
            "preamp_db",
            "band0_db",
            "band1_db",
            "band2_db",
            "band3_db",
            "band4_db",
            "band5_db",
            "band6_db",
            "band7_db",
            "band8_db",
            "band9_db",
            "clip_protect",
        ];
        for missing in ALL_KEYS {
            let content = render_equalizer_file(&EqSettings {
                enabled: true,
                preset: Preset::Rock,
                ..EqSettings::default()
            })
            .lines()
            .filter(|line| !line.starts_with(&format!("{missing}=")))
            .collect::<Vec<_>>()
            .join("\n");
            let error = parse_equalizer_file(content.as_bytes())
                .expect_err("an omitted key must be malformed");
            assert_eq!(error, missing, "the diagnostic names the missing key");
        }
    }

    #[test]
    fn missing_schema_version_is_rejected() {
        let content = render_equalizer_file(&EqSettings::default())
            .lines()
            .filter(|line| !line.starts_with("schema_version="))
            .collect::<Vec<_>>()
            .join("\n");
        let error = parse_equalizer_file(content.as_bytes()).expect_err("missing schema");
        assert_eq!(error, "schema_version");
    }

    #[test]
    fn non_finite_gain_is_malformed() {
        let content = render_equalizer_file(&EqSettings::default())
            .replace("band0_db=\"0.0\"", "band0_db=\"nan\"");
        assert!(parse_equalizer_file(content.as_bytes()).is_err());
    }
}
