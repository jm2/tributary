//! Change handlers for the equalizer settings panel.
//!
//! Every mutation goes through one choke point: read the settings the
//! output holds, apply the delta, push the whole typed struct back
//! through `apply_equalizer_settings`. Persistence (debounce, default
//! suppression) is owned by the audio module, not the UI.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

use crate::audio::equalizer::{ClipProtection, Preset};

use super::widgets::{preset_from_menu_position, preset_menu_position, snap_gain};
use super::{EqualizerControls, SharedAudioOutput};

/// Connect every control's change handler through the single
/// `apply_equalizer_settings` choke point.
pub(super) fn wire_equalizer_controls(
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
        &controls.preset_dropdown,
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
        // `Custom` is neither activatable nor selectable in the list;
        // guard anyway so a programmatic selection can never be
        // mistaken for a menu choice, and restore the named-preset
        // position.
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
/// drift from the applied state), then apply. Every manual edit also
/// moves the preset combo to `Custom` (contract acceptance 5: the
/// persisted `preset` field becomes `custom` and the UI combo displays
/// `Custom`) — the combo update runs under the same re-entrancy guard
/// so it cannot be mistaken for a menu choice.
fn wire_gain_sliders(
    active_output: &SharedAudioOutput,
    preset_dropdown: &gtk::DropDown,
    preamp_scale: &gtk::Scale,
    band_scales: &[gtk::Scale],
    updating: &Rc<Cell<bool>>,
) {
    {
        let output_for_preamp = active_output.clone();
        let updating_for_preamp = updating.clone();
        let preset_dropdown_for_preamp = preset_dropdown.clone();
        preamp_scale.connect_value_changed(move |scale| {
            if updating_for_preamp.get() {
                return;
            }
            let mut settings = output_for_preamp.borrow().equalizer_settings();
            settings.preamp_db = snap_gain(scale.value());
            settings.mark_custom();
            updating_for_preamp.set(true);
            scale.set_value(settings.preamp_db);
            preset_dropdown_for_preamp.set_selected(preset_menu_position(Preset::Custom));
            updating_for_preamp.set(false);
            output_for_preamp
                .borrow()
                .apply_equalizer_settings(settings);
        });
    }

    for (index, scale) in band_scales.iter().enumerate() {
        let output_for_band = active_output.clone();
        let updating_for_band = updating.clone();
        let preset_dropdown_for_band = preset_dropdown.clone();
        scale.connect_value_changed(move |scale| {
            if updating_for_band.get() {
                return;
            }
            let mut settings = output_for_band.borrow().equalizer_settings();
            settings.bands_db[index] = snap_gain(scale.value());
            settings.mark_custom();
            updating_for_band.set(true);
            scale.set_value(settings.bands_db[index]);
            preset_dropdown_for_band.set_selected(preset_menu_position(Preset::Custom));
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
