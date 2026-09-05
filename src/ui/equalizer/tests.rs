//! Tests for the equalizer settings panel: menu-position mappings,
//! gain snapping, persistence-key stability, per-output explanations,
//! and localization coverage across every catalog.

use super::widgets::{
    preset_from_menu_position, preset_menu_position, snap_gain, translated_clip_label,
    unsupported_explanation_for_output_type,
};
use super::{CLIP_KEYS, PRESET_KEYS};
use crate::audio::equalizer::{EqSettings, Preset, MAX_GAIN_DB, MIN_GAIN_DB};

#[test]
fn menu_positions_cover_the_six_contract_entries() {
    assert_eq!(preset_menu_position(Preset::Flat), 0);
    assert_eq!(preset_menu_position(Preset::Pop), 1);
    assert_eq!(preset_menu_position(Preset::Rock), 2);
    assert_eq!(preset_menu_position(Preset::Jazz), 3);
    assert_eq!(preset_menu_position(Preset::Classical), 4);
    assert_eq!(preset_menu_position(Preset::Custom), 5);
    assert_eq!(Preset::CUSTOM_MENU_POSITION, 5);
}

#[test]
fn custom_menu_position_is_never_a_selectable_choice() {
    for position in 0..=5u32 {
        let preset = preset_from_menu_position(position);
        if position == Preset::CUSTOM_MENU_POSITION as u32 {
            assert!(preset.is_none(), "Custom must not be a menu choice");
        } else {
            assert!(preset.is_some());
        }
    }
}

/// Regression (contract acceptance 5, UI-state half): a manual preamp
/// or band edit must move the preset combo to the `Custom` position at
/// the same moment the applied state is marked custom. The wiring
/// syncs the combo through `preset_menu_position(Preset::Custom)`, so
/// that mapping must land exactly on the designated custom position —
/// which itself is never a selectable menu choice, meaning the combo
/// can only reach it through a manual edit, never through the menu.
#[test]
fn manual_edit_marks_custom_and_the_combo_follows_the_custom_position() {
    let mut settings = EqSettings {
        preset: Preset::Rock,
        ..EqSettings::default()
    };
    // A named preset sits on its own selectable menu position…
    assert_eq!(preset_menu_position(settings.preset), 2);

    // …the state transition `wire_gain_sliders` performs on every
    // drag, before applying the settings…
    settings.mark_custom();
    assert_eq!(settings.preset, Preset::Custom);

    // …and the combo sync `wire_gain_sliders` performs under the same
    // re-entrancy guard: the dropdown lands on the custom position.
    assert_eq!(
        preset_menu_position(settings.preset),
        Preset::CUSTOM_MENU_POSITION as u32
    );
    assert!(
        preset_from_menu_position(Preset::CUSTOM_MENU_POSITION as u32).is_none(),
        "Custom must not be a selectable choice, so a custom-position combo \
         can only reflect a manual slider edit"
    );
}

#[test]
fn gain_snapping_stays_on_the_half_step_grid_and_clamps() {
    assert_eq!(snap_gain(-24.4), MIN_GAIN_DB);
    assert_eq!(snap_gain(12.4), MAX_GAIN_DB);
    assert_eq!(snap_gain(-6.25), -6.5);
    assert_eq!(snap_gain(3.24), 3.0);
    assert_eq!(snap_gain(0.26), 0.5);
}

#[test]
fn preset_keys_match_persistence_keys_in_menu_order() {
    assert_eq!(
        PRESET_KEYS,
        ["flat", "pop", "rock", "jazz", "classical", "custom"]
    );
}

#[test]
fn clip_keys_preserve_the_position_based_policy_mapping() {
    assert_eq!(CLIP_KEYS, ["off", "soft"]);
    assert_eq!(
        translated_clip_label(CLIP_KEYS[1]),
        rust_i18n::t!("equalizer.clip_soft").to_string()
    );
    assert_eq!(
        translated_clip_label(CLIP_KEYS[0]),
        rust_i18n::t!("equalizer.clip_off").to_string()
    );
}

#[test]
fn unsupported_explanations_are_per_output_type() {
    use crate::audio::output::OutputType;
    assert_eq!(
        unsupported_explanation_for_output_type(OutputType::AirPlay),
        "equalizer.unsupported.airplay"
    );
    assert_eq!(
        unsupported_explanation_for_output_type(OutputType::Chromecast),
        "equalizer.unsupported.chromecast"
    );
    assert_eq!(
        unsupported_explanation_for_output_type(OutputType::Mpd),
        "equalizer.unsupported.mpd"
    );
    assert_eq!(
        unsupported_explanation_for_output_type(OutputType::Local),
        "equalizer.unsupported.local"
    );
}

/// Every equalizer key that must resolve in every catalog.
const LOCALIZED_KEYS: [&str; 21] = [
    "equalizer.title",
    "equalizer.description",
    "equalizer.enabled",
    "equalizer.preset",
    "equalizer.preset_flat",
    "equalizer.preset_pop",
    "equalizer.preset_rock",
    "equalizer.preset_jazz",
    "equalizer.preset_classical",
    "equalizer.preset_custom",
    "equalizer.preamp",
    "equalizer.clip_protection",
    "equalizer.clip_off",
    "equalizer.clip_soft",
    "equalizer.reset_flat",
    "equalizer.reload",
    "equalizer.band_a11y",
    "equalizer.unsupported.local",
    "equalizer.unsupported.airplay",
    "equalizer.unsupported.chromecast",
    "equalizer.unsupported.mpd",
];

/// Sentence-like keys that must not silently fall back to English.
/// The short preset/protection names are common words or music
/// loanwords (Flat, Pop, Off…) that legitimately coincide across
/// locales, so they are only required to be non-empty.
const SENTENCE_KEYS: [&str; 12] = [
    "equalizer.description",
    "equalizer.enabled",
    "equalizer.preset",
    "equalizer.preamp",
    "equalizer.clip_protection",
    "equalizer.reset_flat",
    "equalizer.reload",
    "equalizer.band_a11y",
    "equalizer.unsupported.local",
    "equalizer.unsupported.airplay",
    "equalizer.unsupported.chromecast",
    "equalizer.unsupported.mpd",
];

#[test]
fn equalizer_strings_are_localized_for_every_catalog() {
    for key in LOCALIZED_KEYS {
        assert_localized_in_every_catalog(key);
    }
}

/// One key resolves in every catalog; sentence-like keys must also
/// differ from English so no catalog silently falls back.
fn assert_localized_in_every_catalog(key: &str) {
    let english = rust_i18n::t!(key, locale = "en");
    assert!(!english.is_empty(), "{key} is empty in en");

    for locale in rust_i18n::available_locales!() {
        let localized = rust_i18n::t!(key, locale = locale);
        assert!(!localized.is_empty(), "{key} is empty for {locale}");
        if locale != "en" && SENTENCE_KEYS.contains(&key) {
            assert_ne!(
                localized, english,
                "{key} must not fall back to English for {locale}"
            );
        }
    }
}
