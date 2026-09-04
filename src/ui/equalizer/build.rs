//! Row builders for the equalizer settings panel: one constructor per
//! contract control, plus the unsupported-output rendering, assembled
//! by [`build_equalizer_group`].

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

use crate::audio::equalizer::{ClipProtection, EqSettings, BAND_CENTERS_HZ};

use super::widgets::{
    announce_gain_value, gain_scale, install_preset_factory, install_translated_choice_factory,
    preset_menu_position, translated_clip_label, unsupported_explanation_of,
};
use super::{EqualizerControls, SharedAudioOutput, CLIP_KEYS, PRESET_KEYS};

/// Build the equalizer preferences group for the given active output.
pub fn build_equalizer_group(active_output: &SharedAudioOutput) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(rust_i18n::t!("equalizer.title").as_ref())
        .description(rust_i18n::t!("equalizer.description").as_ref())
        .build();

    let settings: EqSettings = active_output.borrow().equalizer_settings();
    let supported = active_output.borrow().supports_equalizer();

    // Re-entrancy guard: programmatic control updates (preset load,
    // reload-from-disk, reset) must not be re-interpreted as manual
    // edits — a manual edit is what flips the preset to `Custom`.
    let updating = Rc::new(Cell::new(false));

    let enable_row = build_enable_row(&settings);
    let (preset_row, preset_dropdown) = build_preset_row(&settings);
    let (preamp_row, preamp_scale) = build_preamp_row(&settings);
    let band_scales = build_band_rows(&group, &settings);
    let (clip_row, clip_dropdown) = build_clip_row(&settings);
    let (buttons_row, reset_button, reload_button) = build_buttons_row();

    // Assemble the group (band rows were appended by their builder, so
    // the remaining rows land after them in contract order).
    group.add(&enable_row);
    group.add(&preset_row);
    group.add(&preamp_row);
    group.add(&clip_row);
    group.add(&buttons_row);
    group.add(&build_unsupported_note(active_output, supported));

    let controls = EqualizerControls {
        disabled_rows: vec![
            enable_row.clone().upcast(),
            preset_row.upcast(),
            preamp_row.upcast(),
            clip_row.upcast(),
            buttons_row.upcast(),
        ],
        enable_row,
        preset_dropdown,
        preamp_scale,
        band_scales,
        clip_dropdown,
        reset_button,
        reload_button,
    };
    apply_unsupported_rendering(active_output, supported, &controls);
    super::wiring::wire_equalizer_controls(active_output, &controls, &updating);

    group
}

/// The enable switch row.
fn build_enable_row(settings: &EqSettings) -> adw::SwitchRow {
    adw::SwitchRow::builder()
        .title(rust_i18n::t!("equalizer.enabled").as_ref())
        .active(settings.enabled)
        .build()
}

/// The preset row with its fixed six-entry combo.
fn build_preset_row(settings: &EqSettings) -> (adw::ActionRow, gtk::DropDown) {
    let preset_model = gtk::StringList::new(&PRESET_KEYS);
    let preset_dropdown = gtk::DropDown::builder().model(&preset_model).build();
    install_preset_factory(&preset_dropdown);
    preset_dropdown.set_selected(preset_menu_position(settings.preset));

    let preset_row = adw::ActionRow::builder()
        .title(rust_i18n::t!("equalizer.preset").as_ref())
        .build();
    preset_row.add_suffix(&preset_dropdown);
    (preset_row, preset_dropdown)
}

/// The preamp row with its gain slider.
fn build_preamp_row(settings: &EqSettings) -> (adw::ActionRow, gtk::Scale) {
    let preamp_scale = gain_scale(settings.preamp_db);
    announce_gain_value(&preamp_scale);

    let preamp_row = adw::ActionRow::builder()
        .title(rust_i18n::t!("equalizer.preamp").as_ref())
        .build();
    preamp_row.add_suffix(&preamp_scale);
    (preamp_row, preamp_scale)
}

/// The ten fixed-frequency band rows with their gain sliders, appended
/// to the group in canonical order.
fn build_band_rows(group: &adw::PreferencesGroup, settings: &EqSettings) -> Vec<gtk::Scale> {
    let mut band_scales = Vec::with_capacity(10);
    for (index, center_hz) in BAND_CENTERS_HZ.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(format!("{center_hz} Hz"))
            .build();
        // Announce band, unit, and boundary: "… hertz, decibels" with
        // the slider itself carrying the numeric value interface.
        let band_description = rust_i18n::t!("equalizer.band_a11y", frequency = *center_hz);
        row.upcast_ref::<gtk::Widget>()
            .update_property(&[gtk::accessible::Property::Description(
                band_description.as_ref(),
            )]);
        let scale = gain_scale(settings.bands_db[index]);
        announce_gain_value(&scale);
        row.add_suffix(&scale);
        group.add(&row);
        band_scales.push(scale);
    }
    band_scales
}

/// The clip-protection row with its localized two-entry combo.
fn build_clip_row(settings: &EqSettings) -> (adw::ActionRow, gtk::DropDown) {
    let clip_model = gtk::StringList::new(&CLIP_KEYS);
    let clip_dropdown = gtk::DropDown::builder()
        .model(&clip_model)
        .selected(match settings.clip_protection {
            ClipProtection::Off => 0,
            ClipProtection::Soft => 1,
        })
        .build();
    install_translated_choice_factory(&clip_dropdown, &CLIP_KEYS, translated_clip_label, |_| true);
    let clip_row = adw::ActionRow::builder()
        .title(rust_i18n::t!("equalizer.clip_protection").as_ref())
        .build();
    clip_row.add_suffix(&clip_dropdown);
    (clip_row, clip_dropdown)
}

/// The contract affordances row: Reset to Flat and Reload from disk.
fn build_buttons_row() -> (adw::ActionRow, gtk::Button, gtk::Button) {
    let reset_button = gtk::Button::builder()
        .label(rust_i18n::t!("equalizer.reset_flat").as_ref())
        .css_classes(["flat"])
        .build();
    let reload_button = gtk::Button::builder()
        .label(rust_i18n::t!("equalizer.reload").as_ref())
        .css_classes(["flat"])
        .build();
    let buttons_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    buttons_box.append(&reset_button);
    buttons_box.append(&reload_button);
    let buttons_row = adw::ActionRow::builder().build();
    buttons_row.add_suffix(&buttons_box);
    (buttons_row, reset_button, reload_button)
}

/// The dimmed closed-form explanation shown only for unsupported
/// active outputs.
fn build_unsupported_note(active_output: &SharedAudioOutput, supported: bool) -> gtk::Label {
    let unsupported_note_text = unsupported_explanation_of(active_output.borrow().as_ref());
    gtk::Label::builder()
        .label(&unsupported_note_text)
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .visible(!supported)
        .build()
}

/// Disable every control with a tooltip explanation of why the active
/// output cannot render the equalizer DSP. The persisted values stay
/// visible (and remain on disk); the controls just cannot be touched
/// while the receiver renders audio.
fn apply_unsupported_rendering(
    active_output: &SharedAudioOutput,
    supported: bool,
    controls: &EqualizerControls,
) {
    if supported {
        return;
    }
    let tooltip = unsupported_explanation_of(active_output.borrow().as_ref());
    for widget in &controls.disabled_rows {
        widget.set_sensitive(false);
        widget.set_tooltip_text(Some(&tooltip));
    }
    for scale in &controls.band_scales {
        scale.set_sensitive(false);
    }
    controls.preset_dropdown.set_sensitive(false);
    controls.clip_dropdown.set_sensitive(false);
    controls.preamp_scale.set_sensitive(false);
}
