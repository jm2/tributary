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
    fn absorb(&mut self, key: &str, value: &str) -> Result<(), String> {
        if key == "preamp_db" {
            self.preamp_db = Some(required_gain(value, "preamp_db")?);
            return Ok(());
        }
        if self.absorb_scalar(key, value) {
            return Ok(());
        }
        self.absorb_band(key, value)
    }

    /// Fold one enum-ish scalar key. Returns `false` when the key is not
    /// one of the scalars, leaving it to the band/required-gain handlers.
    fn absorb_scalar(&mut self, key: &str, value: &str) -> bool {
        match key {
            "schema_version" => self.schema_version = Some(value.to_string()),
            "enabled" => self.enabled = Some(value == "true"),
            "preset" => self.preset = Some(Preset::from_key(value)),
            "clip_protect" => self.clip_protection = Some(ClipProtection::from_key(value)),
            _ => return false,
        }
        true
    }

    /// Fold one `band<N>_db` key. Unknown keys are ignored so a future
    /// minor schema can add keys without discarding user state.
    fn absorb_band(&mut self, key: &str, value: &str) -> Result<(), String> {
        if let Some(band) = band_index(key) {
            self.bands_db[band] = Some(parse_gain(value).ok_or_else(|| format!("band{band}_db"))?);
        }
        Ok(())
    }

    /// Finish the scan: require the schema version *and* all fifteen
    /// keys — a file omitting any key is malformed and reported with the
    /// first missing key — then clamp gains to the contract bounds.
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
        Ok(clamp_gains(settings))
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

/// Clamp preamp and band gains into the contract bounds on read.
fn clamp_gains(mut settings: EqSettings) -> EqSettings {
    settings.preamp_db = EqSettings::clamp_gain_db(settings.preamp_db);
    for gain in &mut settings.bands_db {
        *gain = EqSettings::clamp_gain_db(*gain);
    }
    settings
}

/// Parse the strict `key="value"` grammar. Returns `Err(bad_key)` for a
/// malformed line, a bad schema version, or an unparseable mandatory key.
pub(super) fn parse_equalizer_file(bytes: &[u8]) -> Result<EqSettings, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "schema_version".to_string())?;
    let mut config = RawEqConfig::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = parse_line(line).ok_or_else(|| bad_key_of(line))?;
        config.absorb(&key, &value)?;
    }
    config.into_settings()
}

fn bad_key_of(line: &str) -> String {
    line.split('=')
        .next()
        .unwrap_or("(unknown)")
        .trim()
        .to_string()
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
        assert_eq!(parsed.preset, Preset::Jazz);
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
    fn unknown_preset_coerces_to_flat_and_keeps_the_band_vector() {
        let content = render_equalizer_file(&EqSettings {
            preset: Preset::Rock,
            bands_db: [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            ..EqSettings::default()
        })
        .replace("preset=\"rock\"", "preset=\"loudness2001\"");
        let parsed = parse_equalizer_file(content.as_bytes()).expect("coerced parse");
        assert_eq!(parsed.preset, Preset::Flat);
        assert_eq!(parsed.bands_db[0], 3.0);
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
