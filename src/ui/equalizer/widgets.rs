//! Shared widget and localization helpers for the equalizer settings
//! panel: gain sliders with accessible value announcements, the preset
//! and clip-protection menu mappings, the translated `DropDown`
//! factories, and the unsupported-output explanation.

use adw::prelude::*;
use tracing::warn;

use crate::audio::equalizer::{self, EqSettings, Preset, MAX_GAIN_DB, MIN_GAIN_DB};
use crate::audio::output::{AudioOutput, OutputType};

use super::PRESET_KEYS;

/// One gain slider with the contract's fixed range and half-step
/// precision, drawing its dB value.
pub(super) fn gain_scale(initial_db: f64) -> gtk::Scale {
    let adjustment = gtk::Adjustment::new(
        initial_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB),
        MIN_GAIN_DB,
        MAX_GAIN_DB,
        0.5,
        1.0,
        0.0,
    );
    gtk::Scale::builder()
        .adjustment(&adjustment)
        .draw_value(true)
        .digits(1)
        .value_pos(gtk::PositionType::Left)
        .width_request(240)
        .build()
}

/// Announce current value, unit (decibels), and boundary for a gain
/// slider, refreshed on every change (contract: *Accessibility and
/// localization*).
pub(super) fn announce_gain_value(scale: &gtk::Scale) {
    let sync = |scale: &gtk::Scale| {
        let value = scale.value();
        let value_text = format!("{value:.1} dB");
        scale.update_property(&[
            gtk::accessible::Property::ValueNow(value),
            gtk::accessible::Property::ValueMin(MIN_GAIN_DB),
            gtk::accessible::Property::ValueMax(MAX_GAIN_DB),
            gtk::accessible::Property::ValueText(value_text.as_str()),
        ]);
    };
    sync(scale);
    scale.connect_value_changed(move |scale| {
        sync(scale);
    });
}

/// UI values snap to the contract's 0.5 dB precision even when the
/// platform hands the slider a finer intermediate value.
pub(super) fn snap_gain(value: f64) -> f64 {
    EqSettings::clamp_gain_db((value * 2.0).round() / 2.0)
}

pub(super) fn preset_menu_position(preset: Preset) -> u32 {
    match preset {
        Preset::Flat => 0,
        Preset::Pop => 1,
        Preset::Rock => 2,
        Preset::Jazz => 3,
        Preset::Classical => 4,
        Preset::Custom => Preset::CUSTOM_MENU_POSITION as u32,
    }
}

pub(super) fn preset_from_menu_position(position: u32) -> Option<Preset> {
    match position {
        0 => Some(Preset::Flat),
        1 => Some(Preset::Pop),
        2 => Some(Preset::Rock),
        3 => Some(Preset::Jazz),
        4 => Some(Preset::Classical),
        // `Custom` is displayed but never a menu choice.
        _ => None,
    }
}

/// Install the preset combo's factory: translated preset names, with
/// the `Custom` row visible but not activatable — the five named
/// values are the only selectable entries.
pub(super) fn install_preset_factory(dropdown: &gtk::DropDown) {
    install_translated_choice_factory(
        dropdown,
        &PRESET_KEYS,
        translated_preset_label,
        // `Custom` is write-only: shown for context, never clickable.
        |key| key != "custom",
    );
}

/// Install a factory that renders translated labels for a StringList-
/// backed `DropDown`. Without a custom factory GTK displays the literal
/// model values, which would leak the English persistence keys.
/// `activatable` decides per key whether the row can be selected; the
/// combo selection logic keeps reading stable positions while the
/// visible label carries the localized name.
pub(super) fn install_translated_choice_factory(
    dropdown: &gtk::DropDown,
    keys: &'static [&'static str],
    label_of: fn(&str) -> String,
    activatable: fn(&str) -> bool,
) {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            warn!("choice factory setup received a non-list item");
            return;
        };
        let label = gtk::Label::new(None);
        label.set_halign(gtk::Align::Start);
        label.set_margin_start(6);
        list_item.set_child(Some(&label));
    });
    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            warn!("choice factory bind received a non-list item");
            return;
        };
        let Some(label) = list_item.child().and_downcast::<gtk::Label>() else {
            return;
        };
        let position = list_item.position() as usize;
        let key = keys.get(position).copied().unwrap_or(keys[0]);
        let text = label_of(key);
        label.set_text(&text);
        label.set_tooltip_text(Some(&text));
        list_item.set_activatable(activatable(key));
    });
    dropdown.set_factory(Some(&factory));
}

fn translated_preset_label(key: &str) -> String {
    match key {
        "flat" => rust_i18n::t!("equalizer.preset_flat").to_string(),
        "pop" => rust_i18n::t!("equalizer.preset_pop").to_string(),
        "rock" => rust_i18n::t!("equalizer.preset_rock").to_string(),
        "jazz" => rust_i18n::t!("equalizer.preset_jazz").to_string(),
        "classical" => rust_i18n::t!("equalizer.preset_classical").to_string(),
        _ => rust_i18n::t!("equalizer.preset_custom").to_string(),
    }
}

pub(super) fn translated_clip_label(key: &str) -> String {
    match key {
        "soft" => rust_i18n::t!("equalizer.clip_soft").to_string(),
        _ => rust_i18n::t!("equalizer.clip_off").to_string(),
    }
}

/// Closed-form explanation for the active output's capability row.
pub(super) fn unsupported_explanation_of(output: &dyn AudioOutput) -> String {
    match equalizer::unsupported_explanation_key(output.output_type()) {
        "equalizer.unsupported.airplay" => {
            rust_i18n::t!("equalizer.unsupported.airplay").into_owned()
        }
        "equalizer.unsupported.chromecast" => {
            rust_i18n::t!("equalizer.unsupported.chromecast").into_owned()
        }
        "equalizer.unsupported.mpd" => rust_i18n::t!("equalizer.unsupported.mpd").into_owned(),
        _ => rust_i18n::t!("equalizer.unsupported.local").into_owned(),
    }
}

/// Map an output type to its explanation key without a live output.
/// Kept pure for tests.
#[allow(dead_code)] // test seam over the shared capability key mapping
pub fn unsupported_explanation_for_output_type(output_type: OutputType) -> &'static str {
    equalizer::unsupported_explanation_key(output_type)
}
