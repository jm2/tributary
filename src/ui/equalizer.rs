//! Equalizer settings panel — the accessible UI surface of the
//! equalizer contract (docs/equalizer.md, issue #49).
//!
//! The panel exposes exactly the bounded control set: an enable switch,
//! a preset combo (five named values plus a non-selectable `Custom`
//! entry), a preamp slider, ten fixed-frequency band sliders, a clip
//! protection combo, and the two contract affordances (Reset to Flat,
//! Reload from disk). When the active output reports
//! [`AudioOutput::supports_equalizer`](crate::audio::output::AudioOutput::supports_equalizer)
//! as `false`, every control renders disabled with a closed-form
//! explanation of why the receiver cannot be reached.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use tracing::warn;

use crate::audio::equalizer::{
    self, ClipProtection, EqSettings, Preset, BAND_CENTERS_HZ, MAX_GAIN_DB, MIN_GAIN_DB,
};
use crate::audio::output::{AudioOutput, OutputType};

/// Shared active-output handle (same shape as the window's
/// `SharedAudioOutput`).
pub type SharedAudioOutput = Rc<std::cell::RefCell<Box<dyn AudioOutput>>>;

/// Menu positions in the fixed six-entry preset combo. `Custom` is the
/// sixth entry: shown, never activatable.
const PRESET_KEYS: [&str; 6] = ["flat", "pop", "rock", "jazz", "classical", "custom"];

/// Menu keys of the fixed two-entry clip-protection combo. Position
/// 0 = `Off`, 1 = `Soft`; the position-based `ClipProtection` mapping
/// must not change.
const CLIP_KEYS: [&str; 2] = ["off", "soft"];

/// Cloned handles to the built controls, passed to the wiring helpers.
struct EqualizerControls {
    enable_row: adw::SwitchRow,
    preset_dropdown: gtk::DropDown,
    preamp_scale: gtk::Scale,
    band_scales: Vec<gtk::Scale>,
    clip_dropdown: gtk::DropDown,
    reset_button: gtk::Button,
    reload_button: gtk::Button,
    /// The five list rows, disabled together for unsupported outputs.
    disabled_rows: Vec<gtk::Widget>,
}

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
    wire_equalizer_controls(active_output, &controls, &updating);

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

// ── Wiring ──────────────────────────────────────────────────────────────
//
// Every mutation goes through one choke point: read the settings the
// output holds, apply the delta, push the whole typed struct back
// through `apply_equalizer_settings`. Persistence (debounce, default
// suppression) is owned by the audio module, not the UI.

/// Connect every control's change handler through the single
/// `apply_equalizer_settings` choke point.
fn wire_equalizer_controls(
    active_output: &SharedAudioOutput,
    controls: &EqualizerControls,
    updating: &Rc<Cell<bool>>,
) {
    wire_enable_switch(active_output, &controls.enable_row, updating);
    wire_preset_dropdown(
        active_output,
        &controls.preset_dropdown,
        &controls.band_scales,
        &controls.preamp_scale,
        updating,
    );
    wire_gain_sliders(
        active_output,
        &controls.preamp_scale,
        &controls.band_scales,
        updating,
    );
    wire_clip_dropdown(active_output, &controls.clip_dropdown, updating);
    wire_reset_button(active_output, controls, updating);
    wire_reload_button(active_output, controls, updating);
}

/// Enable switch: flip the typed state's `enabled` flag.
fn wire_enable_switch(
    active_output: &SharedAudioOutput,
    enable_row: &adw::SwitchRow,
    updating: &Rc<Cell<bool>>,
) {
    let active_output = active_output.clone();
    let updating = updating.clone();
    enable_row.connect_active_notify(move |row| {
        if updating.get() {
            return;
        }
        let mut settings = active_output.borrow().equalizer_settings();
        settings.enabled = row.is_active();
        active_output.borrow().apply_equalizer_settings(settings);
    });
}

/// Preset combo: load the named preset's band vector and preamp, then
/// reflect the full preset write across the sliders.
fn wire_preset_dropdown(
    active_output: &SharedAudioOutput,
    preset_dropdown: &gtk::DropDown,
    band_scales: &[gtk::Scale],
    preamp_scale: &gtk::Scale,
    updating: &Rc<Cell<bool>>,
) {
    let active_output = active_output.clone();
    let updating = updating.clone();
    let band_scales = band_scales.to_vec();
    let preamp_scale = preamp_scale.clone();
    preset_dropdown.connect_selected_notify(move |dropdown| {
        if updating.get() {
            return;
        }
        let position = dropdown.selected();
        // `Custom` is not activatable in the list; guard anyway so a
        // programmatic selection can never be mistaken for a menu
        // choice, and restore the named-preset position.
        let Some(preset) = preset_from_menu_position(position) else {
            return;
        };
        let mut settings = active_output.borrow().equalizer_settings();
        settings.preset = preset;
        settings.bands_db = preset.band_gains_db();
        settings.preamp_db = preset.recommended_preamp_db();
        active_output.borrow().apply_equalizer_settings(settings);
        updating.set(true);
        for (scale, gain) in band_scales.iter().zip(settings.bands_db) {
            scale.set_value(gain);
        }
        preamp_scale.set_value(settings.preamp_db);
        updating.set(false);
    });
}

/// Preamp and band sliders: snap the dragged value to the contract's
/// half-step grid, mirror the snapped DSP value back onto the
/// originating slider (so the visible and accessible values cannot
/// drift from the applied state), then apply.
fn wire_gain_sliders(
    active_output: &SharedAudioOutput,
    preamp_scale: &gtk::Scale,
    band_scales: &[gtk::Scale],
    updating: &Rc<Cell<bool>>,
) {
    {
        let output_for_preamp = active_output.clone();
        let updating_for_preamp = updating.clone();
        preamp_scale.connect_value_changed(move |scale| {
            if updating_for_preamp.get() {
                return;
            }
            let mut settings = output_for_preamp.borrow().equalizer_settings();
            settings.preamp_db = snap_gain(scale.value());
            settings.mark_custom();
            updating_for_preamp.set(true);
            scale.set_value(settings.preamp_db);
            updating_for_preamp.set(false);
            output_for_preamp
                .borrow()
                .apply_equalizer_settings(settings);
        });
    }

    for (index, scale) in band_scales.iter().enumerate() {
        let output_for_band = active_output.clone();
        let updating_for_band = updating.clone();
        scale.connect_value_changed(move |scale| {
            if updating_for_band.get() {
                return;
            }
            let mut settings = output_for_band.borrow().equalizer_settings();
            settings.bands_db[index] = snap_gain(scale.value());
            settings.mark_custom();
            updating_for_band.set(true);
            scale.set_value(settings.bands_db[index]);
            updating_for_band.set(false);
            output_for_band.borrow().apply_equalizer_settings(settings);
        });
    }
}

/// Clip-protection combo: map the fixed menu position to the policy.
fn wire_clip_dropdown(
    active_output: &SharedAudioOutput,
    clip_dropdown: &gtk::DropDown,
    updating: &Rc<Cell<bool>>,
) {
    let active_output = active_output.clone();
    let updating = updating.clone();
    clip_dropdown.connect_selected_notify(move |dropdown| {
        if updating.get() {
            return;
        }
        let mut settings = active_output.borrow().equalizer_settings();
        settings.clip_protection = match dropdown.selected() {
            1 => ClipProtection::Soft,
            _ => ClipProtection::Off,
        };
        active_output.borrow().apply_equalizer_settings(settings);
    });
}

/// Reset to Flat: bands and preamp to zero, preset to Flat; Enabled and
/// Clip protection keep their current values.
fn wire_reset_button(
    active_output: &SharedAudioOutput,
    controls: &EqualizerControls,
    updating: &Rc<Cell<bool>>,
) {
    let active_output = active_output.clone();
    let updating = updating.clone();
    let band_scales = controls.band_scales.clone();
    let preamp_scale = controls.preamp_scale.clone();
    let preset_dropdown = controls.preset_dropdown.clone();
    controls.reset_button.connect_clicked(move |_| {
        let mut settings = active_output.borrow().equalizer_settings();
        settings.preset = Preset::Flat;
        settings.bands_db = Preset::Flat.band_gains_db();
        settings.preamp_db = Preset::Flat.recommended_preamp_db();
        active_output.borrow().apply_equalizer_settings(settings);
        updating.set(true);
        for scale in &band_scales {
            scale.set_value(0.0);
        }
        preamp_scale.set_value(0.0);
        preset_dropdown.set_selected(preset_menu_position(Preset::Flat));
        updating.set(false);
    });
}

/// Reload from disk: the only escape hatch from a malformed file,
/// performed by the audio module (which owns the path), then reflect
/// the loaded state across every control.
fn wire_reload_button(
    active_output: &SharedAudioOutput,
    controls: &EqualizerControls,
    updating: &Rc<Cell<bool>>,
) {
    let active_output = active_output.clone();
    let updating = updating.clone();
    let enable_row = controls.enable_row.clone();
    let preset_dropdown = controls.preset_dropdown.clone();
    let clip_dropdown = controls.clip_dropdown.clone();
    let band_scales = controls.band_scales.clone();
    let preamp_scale = controls.preamp_scale.clone();
    controls.reload_button.connect_clicked(move |_| {
        let settings = active_output.borrow().reload_equalizer_settings();
        updating.set(true);
        enable_row.set_active(settings.enabled);
        for (scale, gain) in band_scales.iter().zip(settings.bands_db) {
            scale.set_value(gain);
        }
        preamp_scale.set_value(settings.preamp_db);
        preset_dropdown.set_selected(preset_menu_position(settings.preset));
        clip_dropdown.set_selected(match settings.clip_protection {
            ClipProtection::Off => 0,
            ClipProtection::Soft => 1,
        });
        updating.set(false);
    });
}

// ── Widget helpers ──────────────────────────────────────────────────────

/// One gain slider with the contract's fixed range and half-step
/// precision, drawing its dB value.
fn gain_scale(initial_db: f64) -> gtk::Scale {
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
fn announce_gain_value(scale: &gtk::Scale) {
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
fn snap_gain(value: f64) -> f64 {
    EqSettings::clamp_gain_db((value * 2.0).round() / 2.0)
}

fn preset_menu_position(preset: Preset) -> u32 {
    match preset {
        Preset::Flat => 0,
        Preset::Pop => 1,
        Preset::Rock => 2,
        Preset::Jazz => 3,
        Preset::Classical => 4,
        Preset::Custom => Preset::CUSTOM_MENU_POSITION as u32,
    }
}

fn preset_from_menu_position(position: u32) -> Option<Preset> {
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
fn install_preset_factory(dropdown: &gtk::DropDown) {
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
fn install_translated_choice_factory(
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

fn translated_clip_label(key: &str) -> String {
    match key {
        "soft" => rust_i18n::t!("equalizer.clip_soft").to_string(),
        _ => rust_i18n::t!("equalizer.clip_off").to_string(),
    }
}

/// Closed-form explanation for the active output's capability row.
fn unsupported_explanation_of(output: &dyn AudioOutput) -> String {
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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // snapped gains land on exactly representable half-steps
mod tests {
    use super::*;

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
}
