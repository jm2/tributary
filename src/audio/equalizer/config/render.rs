//! Canonical rendering of the on-disk `equalizer.cfg` content: the
//! contract's keys in order, each value double-quoted, floats with
//! exactly one decimal place.

use crate::audio::equalizer::{ClipProtection, EqSettings, Preset};

use super::SCHEMA_VERSION;

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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
