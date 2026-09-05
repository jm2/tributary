//! Equalizer persistence: the `equalizer.cfg` grammar, strict parsing,
//! and the atomic-replace writer (contract: *Persistence format*).

use std::path::PathBuf;

use super::{ClipProtection, EqSettings, Preset};

/// The only supported on-disk schema version.
const SCHEMA_VERSION: &str = "1";

// ── Render ──────────────────────────────────────────────────────────────

/// Append one `key="value"` line with the contract's escaping (`\"`,
/// `\\`) and no trailing newline skipping — every value is quoted.
fn push_quoted_line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str("=\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push_str("\"\n");
}

/// Render the canonical on-disk content: six keys in contract order, each
/// value double-quoted, floats with exactly one decimal place.
pub fn render_equalizer_file(settings: &EqSettings) -> String {
    let mut out = String::with_capacity(256);
    push_quoted_line(&mut out, "schema_version", SCHEMA_VERSION);
    push_quoted_line(&mut out, "enabled", &settings.enabled.to_string());
    push_quoted_line(&mut out, "preset", Preset::key(settings.preset));
    push_quoted_line(&mut out, "preamp_db", &format!("{:.1}", settings.preamp_db));
    for (index, gain) in settings.bands_db.iter().enumerate() {
        push_quoted_line(
            &mut out,
            &format!("band{index}_db"),
            &format!("{:.1}", gain),
        );
    }
    push_quoted_line(
        &mut out,
        "clip_protect",
        ClipProtection::key(settings.clip_protection),
    );
    out
}

// ── Load ────────────────────────────────────────────────────────────────

/// Path to the equalizer state file: `<data_dir>/tributary/equalizer.cfg`,
/// beside the `volume` file. Owned exclusively by this module.
fn equalizer_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("equalizer.cfg"))
}

/// What happened when the on-disk state was read.
#[derive(Debug)]
pub enum EqLoadOutcome {
    /// The file was valid (coercions and clamps may still have been
    /// applied per the validation rules), or no file exists (fresh
    /// install).
    Loaded(EqSettings),
    /// The file was malformed or carried an unsupported schema version.
    /// Defaults are returned and have been re-written to disk via the
    /// same atomic-replace protocol.
    ReplacedWithDefaults {
        settings: EqSettings,
        diagnostic: EqFileDiagnostic,
    },
    /// The file exists but could not be read (permissions, transient
    /// I/O). This is *not* a malformed file: defaults run in memory for
    /// the session and the on-disk bytes are left exactly as they are.
    /// The caller must suppress every persistence path — the debounced
    /// writer and the shutdown flush — until a subsequent read succeeds
    /// and reconciles the in-memory state with disk, so a valid file can
    /// never be overwritten with defaults (contract: *Persistence*,
    /// transient read failure).
    TransientReadFailure(EqSettings),
}

/// Coarse outcome of a settings load, for callers that must react to a
/// transient read failure (suppressing persistence) without carrying
/// the full outcome payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqLoadStatus {
    Loaded,
    ReplacedWithDefaults,
    TransientReadFailure,
}

/// Bounded diagnostic for a replaced malformed file: file path, byte
/// count, and the offending key only — never the file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqFileDiagnostic {
    pub path: String,
    pub byte_count: u64,
    pub bad_key: String,
}

/// Shared reader for startup and the settings UI's reload escape hatch:
/// load the persisted state, replace a malformed file, and report
/// exactly one bounded diagnostic per replacement or transient failure.
/// Both callers must share this path so the diagnostic wording cannot
/// drift. The returned status tells the caller whether persistence must
/// stay suppressed (transient read failure) until a successful read.
pub fn load_settings_with_status() -> (EqSettings, EqLoadStatus) {
    match load_equalizer_settings_from_disk() {
        EqLoadOutcome::Loaded(settings) => (settings, EqLoadStatus::Loaded),
        EqLoadOutcome::ReplacedWithDefaults {
            settings,
            diagnostic,
        } => {
            tracing::warn!(
                path = %diagnostic.path,
                byte_count = diagnostic.byte_count,
                bad_key = %diagnostic.bad_key,
                "Malformed equalizer.cfg replaced with default state"
            );
            (settings, EqLoadStatus::ReplacedWithDefaults)
        }
        EqLoadOutcome::TransientReadFailure(settings) => {
            tracing::warn!(
                path = %equalizer_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                "equalizer.cfg exists but is unreadable; persistence is suppressed \
                 until a successful read reconciles state with disk"
            );
            (settings, EqLoadStatus::TransientReadFailure)
        }
    }
}

/// Read `equalizer.cfg`, validate, clamp, and coerce per the contract.
pub fn load_equalizer_settings_from_disk() -> EqLoadOutcome {
    match equalizer_path() {
        Some(path) => load_equalizer_settings_from_path(&path),
        None => EqLoadOutcome::Loaded(EqSettings::default()),
    }
}

/// Path-injected load used by startup and by tests: every caller must
/// share this validation path so on-disk semantics cannot drift.
fn load_equalizer_settings_from_path(path: &std::path::Path) -> EqLoadOutcome {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // Missing file is the fresh-install shape: defaults, no write.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EqLoadOutcome::Loaded(EqSettings::default());
        }
        Err(_) => {
            // Unreadable (permissions, transient I/O): run with defaults
            // in memory, keep the on-disk state, and let the caller
            // suppress persistence. A read that never succeeded cannot
            // establish that the file is malformed, so the defaults-
            // overwrite path below must stay out of reach — one failed
            // read must never schedule a write over valid disk state.
            return EqLoadOutcome::TransientReadFailure(EqSettings::default());
        }
    };
    match parse_equalizer_file(&bytes) {
        Ok(settings) => EqLoadOutcome::Loaded(settings),
        Err(bad_key) => replace_with_defaults(path, bytes.len() as u64, &bad_key),
    }
}

fn replace_with_defaults(path: &std::path::Path, byte_count: u64, bad_key: &str) -> EqLoadOutcome {
    let settings = EqSettings::default();
    let diagnostic = EqFileDiagnostic {
        path: path.display().to_string(),
        byte_count,
        bad_key: bad_key.to_string(),
    };
    let _ = write_equalizer_file_atomic(path, &render_equalizer_file(&settings));
    EqLoadOutcome::ReplacedWithDefaults {
        settings,
        diagnostic,
    }
}

// ── Parse ───────────────────────────────────────────────────────────────

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
        let schema_version = self
            .schema_version
            .ok_or_else(|| "schema_version".to_string())?;
        if schema_version != SCHEMA_VERSION {
            return Err("schema_version".to_string());
        }
        let enabled = self.enabled.ok_or_else(|| "enabled".to_string())?;
        let preset = self.preset.ok_or_else(|| "preset".to_string())?;
        let preamp_db = self.preamp_db.ok_or_else(|| "preamp_db".to_string())?;
        let mut bands_db = [0.0; 10];
        for (index, gain) in bands_db.iter_mut().enumerate() {
            *gain = self.bands_db[index].ok_or_else(|| format!("band{index}_db"))?;
        }
        let clip_protection = self
            .clip_protection
            .ok_or_else(|| "clip_protect".to_string())?;
        let mut settings = EqSettings {
            enabled,
            preset,
            preamp_db,
            bands_db,
            clip_protection,
        };
        settings.preamp_db = EqSettings::clamp_gain_db(settings.preamp_db);
        for gain in &mut settings.bands_db {
            *gain = EqSettings::clamp_gain_db(*gain);
        }
        Ok(settings)
    }
}

/// Parse the strict `key="value"` grammar. Returns `Err(bad_key)` for a
/// malformed line, a bad schema version, or an unparseable mandatory key.
fn parse_equalizer_file(bytes: &[u8]) -> Result<EqSettings, String> {
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

// ── Save ────────────────────────────────────────────────────────────────

/// Persist settings with the atomic-replace protocol: temp file
/// (`O_EXCL`, single write, fsync), `rename(2)`, then directory fsync.
/// A concurrent or failed writer never leaves a partial file visible.
pub fn save_equalizer_settings_to_disk(settings: &EqSettings) -> bool {
    let Some(path) = equalizer_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_equalizer_file_atomic(&path, &render_equalizer_file(settings)).is_ok()
}

fn write_equalizer_file_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let temp_path = temp_sibling(path);
    let write_result = write_temp_then_rename(&temp_path, path, content);
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

/// Write `content` to `temp_path` (O_EXCL, single write, fsync), then
/// `rename(2)` it onto `path` and fsync the directory entry.
fn write_temp_then_rename(
    temp_path: &std::path::Path,
    path: &std::path::Path,
    content: &str,
) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = open_temp_file(temp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temp_path, path)?;
    // The rename alone is not durable until the directory entry is.
    sync_parent_dir(path);
    Ok(())
}

/// Create the temp sibling for exclusive single-writer access.
fn open_temp_file(temp_path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(temp_path)
}

/// Flush the directory entry so the completed rename survives a crash.
fn sync_parent_dir(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn temp_sibling(path: &std::path::Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests {
    use super::super::{MAX_GAIN_DB, MIN_GAIN_DB};
    use super::*;

    // ── Render grammar ──────────────────────────────────────────────

    #[test]
    fn rendered_default_file_matches_the_contract_block() {
        let rendered = render_equalizer_file(&EqSettings::default());
        let expected = "\
schema_version=\"1\"
enabled=\"false\"
preset=\"flat\"
preamp_db=\"0.0\"
band0_db=\"0.0\"
band1_db=\"0.0\"
band2_db=\"0.0\"
band3_db=\"0.0\"
band4_db=\"0.0\"
band5_db=\"0.0\"
band6_db=\"0.0\"
band7_db=\"0.0\"
band8_db=\"0.0\"
band9_db=\"0.0\"
clip_protect=\"off\"
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn rendered_values_carry_one_decimal_and_canonical_order() {
        let settings = EqSettings {
            enabled: true,
            preset: Preset::Pop,
            preamp_db: -2.0,
            bands_db: [1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 12.0],
            clip_protection: ClipProtection::Soft,
        };
        let rendered = render_equalizer_file(&settings);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 15);
        assert_eq!(lines[0], "schema_version=\"1\"");
        assert_eq!(lines[1], "enabled=\"true\"");
        assert_eq!(lines[2], "preset=\"pop\"");
        assert_eq!(lines[3], "preamp_db=\"-2.0\"");
        assert_eq!(lines[4], "band0_db=\"1.0\"");
        assert_eq!(lines[13], "band9_db=\"12.0\"");
        assert_eq!(lines[14], "clip_protect=\"soft\"");
    }

    #[test]
    fn values_are_escaped_like_contract_values() {
        let mut out = String::new();
        push_quoted_line(&mut out, "preset", "sa\"fe\\path");
        assert_eq!(out, "preset=\"sa\\\"fe\\\\path\"\n");
    }

    // ── Parse: round-trip, coercions, clamps ────────────────────────

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

    // ── Atomic writer ───────────────────────────────────────────────

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_sibling() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");

        assert!(write_equalizer_file_atomic(&path, "first=\"1\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first=\"1\"\n");
        assert!(!temp_sibling(&path).exists());

        assert!(write_equalizer_file_atomic(&path, "second=\"2\"\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second=\"2\"\n");
        assert!(!temp_sibling(&path).exists());
    }

    #[test]
    fn temp_sibling_sits_beside_the_destination() {
        let path = std::path::Path::new("state/tributary/equalizer.cfg");
        assert_eq!(
            temp_sibling(path),
            std::path::Path::new("state/tributary/equalizer.cfg.tmp")
        );
    }

    // ── Disk-level load outcomes ────────────────────────────────────

    /// Regression (contract acceptance 18): a *transient* read failure
    /// is not a malformed file. The outcome is
    /// `TransientReadFailure` with in-memory defaults, the on-disk bytes
    /// stay exactly as they were, and no defaults-overwrite is scheduled.
    #[test]
    fn transient_unreadable_file_reports_failure_without_touching_disk() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        // A directory at the config path is portable way to make
        // `std::fs::read` fail with a non-NotFound error on every host.
        std::fs::create_dir(&path).expect("unreadable fixture");

        let outcome = load_equalizer_settings_from_path(&path);
        let settings = match &outcome {
            EqLoadOutcome::TransientReadFailure(settings) => *settings,
            other => panic!("an unreadable file must be a transient failure, not {other:?}"),
        };
        assert_eq!(settings, EqSettings::default());
        // The unreadable fixture is untouched: no atomic replace ran
        // over it, and no temp sibling was scheduled.
        assert!(path.is_dir());
        assert!(!temp_sibling(&path).exists());
    }

    /// The malformed-content path keeps its contract behavior: the
    /// defaults-overwrite repair runs only for a read that *succeeded*,
    /// with the bounded diagnostic carrying the bad key.
    #[test]
    fn malformed_content_is_repaired_at_the_path_level() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        let malformed = "preset=flat\n";
        std::fs::write(&path, malformed).expect("seed malformed file");

        let outcome = load_equalizer_settings_from_path(&path);
        let (settings, diagnostic) = match &outcome {
            EqLoadOutcome::ReplacedWithDefaults {
                settings,
                diagnostic,
            } => (*settings, diagnostic.clone()),
            other => panic!("malformed content must be replaced with defaults, not {other:?}"),
        };
        assert_eq!(settings, EqSettings::default());
        assert_eq!(diagnostic.bad_key, "preset");
        assert_eq!(diagnostic.byte_count, malformed.len() as u64);
        // The repair wrote the default file content back to disk.
        assert_eq!(
            std::fs::read_to_string(&path).ok().as_deref(),
            Some(render_equalizer_file(&EqSettings::default()).as_str())
        );
    }
}
