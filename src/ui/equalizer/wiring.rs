//! Change handlers for the equalizer settings panel.
//!
//! Every mutation goes through one choke point: read the settings the
//! output holds, apply the delta, push the whole typed struct back
//! through `apply_equalizer_settings`. Persistence (debounce, default
//! suppression) is owned by the audio module, not the UI.
//!
//! **Applied-state discipline.** `apply_equalizer_settings` is the
//! authority: it walks the recorded `enabled`/`clip_protection` back to
//! the *installed* topology whenever a deferred seam or a failed limiter
//! surgery refuses the request. Every handler therefore reflects the
//! state the output reports *after* the apply — never the requested
//! delta — so a refused choice cannot leave a widget displaying a value
//! that is not in effect (for example a switch reading `Off` while EQ
//! processing is still active, or a combo reading `Soft` while no
//! limiter is routed). The reflection runs under the shared re-entrancy
//! guard, so the programmatic widget updates are never re-interpreted as
//! manual edits (no recursive callback loop), and a later user retry
//! applies.
//!
//! **Echo safety across notification delivery.** The re-entrancy guard
//! alone is not sufficient on a real display: GTK can deliver the
//! `notify` of a programmatic applied-state reflection *after* the
//! editing handler has unwound (the guard is already cleared again), so
//! the echo would be re-interpreted as a user edit and re-applied. Every
//! reflected control therefore also refuses to apply a request that
//! already equals the recorded state — an echo of our own reflection is
//! a structural no-op no matter when GTK delivers it, while a genuine
//! user edit always differs from the recorded state and still applies
//! exactly once.

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

/// Enable switch: flip the typed state's `enabled` flag, then reflect
/// the **authoritative applied state** back onto the switch. A deferred
/// or rejected install/uninstall walks the recorded `enabled` back to
/// the installed truth (`Player::apply_equalizer_settings`), so the
/// clicked widget must not keep displaying the refused choice — the
/// switch would otherwise read `Off` while EQ processing is still
/// active. The widget sync runs under the shared re-entrancy guard, so
/// the programmatic `set_active` cannot be re-interpreted as a manual
/// edit (no recursive callback loop), and a later user retry applies.
/// Because GTK can deliver the reflection's `notify` *after* the guard
/// is cleared (real-display delivery), the handler additionally skips
/// any request that already equals the recorded state — see the module
/// header's echo-safety contract.
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
        // Echo safety: GTK may deliver the notify of our own applied-state
        // reflection after this handler has unwound (guard already
        // cleared). A request that already equals the recorded state is
        // that echo — never a user edit — so applying it would double-apply
        // and schedule a spurious save. Skip it; a genuine retry always
        // differs from the recorded state.
        let wanted = row.is_active();
        if active_output.borrow().equalizer_settings().enabled == wanted {
            return;
        }
        let mut settings = active_output.borrow().equalizer_settings();
        settings.enabled = wanted;
        active_output.borrow().apply_equalizer_settings(settings);
        // The apply is the authority: a deferred install/uninstall keeps
        // the installed `enabled`, and the switch must show that, not the
        // click.
        let applied = active_output.borrow().equalizer_settings();
        updating.set(true);
        row.set_active(applied.enabled);
        updating.set(false);
    });
}

/// Preset combo: load the named preset's band vector and preamp, apply,
/// then reflect the full preset write across the sliders from the
/// **authoritative applied state**.
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
        let applied = active_output.borrow().equalizer_settings();
        updating.set(true);
        for (scale, gain) in band_scales.iter().zip(applied.bands_db) {
            scale.set_value(gain);
        }
        preamp_scale.set_value(applied.preamp_db);
        updating.set(false);
    });
}

/// Preamp and band sliders: snap the dragged value to the contract's
/// half-step grid, apply, then mirror the applied DSP value back onto
/// the originating slider (so the visible and accessible values cannot
/// drift from the applied state). Every manual edit also moves the
/// preset combo to `Custom` (contract acceptance 5: the persisted
/// `preset` field becomes `custom` and the UI combo displays `Custom`)
/// — sourced from the applied state and running under the same
/// re-entrancy guard so it cannot be mistaken for a menu choice.
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
            output_for_preamp
                .borrow()
                .apply_equalizer_settings(settings);
            let applied = output_for_preamp.borrow().equalizer_settings();
            updating_for_preamp.set(true);
            scale.set_value(applied.preamp_db);
            preset_dropdown_for_preamp.set_selected(preset_menu_position(applied.preset));
            updating_for_preamp.set(false);
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
            output_for_band.borrow().apply_equalizer_settings(settings);
            let applied = output_for_band.borrow().equalizer_settings();
            updating_for_band.set(true);
            scale.set_value(applied.bands_db[index]);
            preset_dropdown_for_band.set_selected(preset_menu_position(applied.preset));
            updating_for_band.set(false);
        });
    }
}

/// Clip-protection combo: map the fixed menu position to the policy,
/// then reflect the **authoritative applied state** back onto the
/// dropdown. `Player::apply_equalizer_settings` records the protection
/// the installed chain actually carries when the toggle is deferred or
/// the limiter surgery fails, so a refused request must not leave the
/// combo showing a policy that is not in effect — the dropdown would
/// otherwise read `Soft` while no limiter is routed (or `Off` while the
/// limiter is still in the bin). The widget sync runs under the shared
/// re-entrancy guard, so the programmatic `set_selected` cannot be
/// re-interpreted as a manual choice (no recursive callback loop), and a
/// later user retry applies. The echo-safety skip (module header) also
/// guards this control: a `selected` notify matching the recorded policy
/// is never a user edit.
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
        // Echo safety (see the module header): the notify of our own
        // applied-state reflection can arrive after the guard is cleared,
        // so a request matching the recorded state must never re-apply.
        let wanted = match dropdown.selected() {
            1 => ClipProtection::Soft,
            _ => ClipProtection::Off,
        };
        if active_output.borrow().equalizer_settings().clip_protection == wanted {
            return;
        }
        let mut settings = active_output.borrow().equalizer_settings();
        settings.clip_protection = wanted;
        active_output.borrow().apply_equalizer_settings(settings);
        let applied = active_output.borrow().equalizer_settings();
        updating.set(true);
        dropdown.set_selected(clip_menu_position(applied.clip_protection));
        updating.set(false);
    });
}

/// Menu position of a clip-protection policy in the fixed two-entry
/// combo (`0 = Off`, `1 = Soft`). Shared by the initial build, the
/// reload path, and the applied-state read-back so the mapping cannot
/// drift between them.
pub(super) fn clip_menu_position(protection: ClipProtection) -> u32 {
    match protection {
        ClipProtection::Off => 0,
        ClipProtection::Soft => 1,
    }
}

/// Reset to Flat: bands and preamp to zero, preset to Flat; Enabled and
/// Clip protection keep their current values. The slider/combo writes
/// reflect the applied state so the compound update stays consistent
/// with every other handler.
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
        let applied = active_output.borrow().equalizer_settings();
        updating.set(true);
        for (scale, gain) in band_scales.iter().zip(applied.bands_db) {
            scale.set_value(gain);
        }
        preamp_scale.set_value(applied.preamp_db);
        preset_dropdown.set_selected(preset_menu_position(applied.preset));
        updating.set(false);
    });
}

/// Reload from disk: the only escape hatch from a malformed file,
/// performed by the audio module (which owns the path), then reflect
/// the loaded state across every control. A supported output reloads
/// through its own state and the panel shows the **authoritative
/// applied state** afterwards; an unsupported active renderer parks the
/// local settings on disk, so the reload surfaces the parked persisted
/// state — the disabled panel must keep showing the last-saved values,
/// never defaults (contract: *Capability matrix*), and the shared
/// reader keeps the repair-and-diagnose behavior identical to
/// startup's.
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
        let settings = {
            let output = active_output.borrow();
            if output.supports_equalizer() {
                output.reload_equalizer_settings();
                // The apply above is the authority; reflect the state the
                // output now reports (a deferred edit keeps the installed
                // topology) rather than the freshly loaded file.
                output.equalizer_settings()
            } else {
                crate::audio::equalizer::config::load_settings_with_status().0
            }
        };
        updating.set(true);
        enable_row.set_active(settings.enabled);
        for (scale, gain) in band_scales.iter().zip(settings.bands_db) {
            scale.set_value(gain);
        }
        preamp_scale.set_value(settings.preamp_db);
        preset_dropdown.set_selected(preset_menu_position(settings.preset));
        clip_dropdown.set_selected(clip_menu_position(settings.clip_protection));
        updating.set(false);
    });
}
