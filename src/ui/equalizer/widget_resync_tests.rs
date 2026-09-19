//! Refinery corrective round 3 (PR 220) resync contracts for the
//! equalizer settings panel: a clip toggle parked across the
//! limiter-edit engagement window leaves the open panel displaying the
//! walked-back value, and the panel-registered resync must move the
//! real combo to the adopted policy without re-applying or flipping the
//! named preset, while an adoption landing the already-displayed value
//! must change nothing.
//!
//! Split verbatim from `widget_tests` (same assertions) to keep both
//! files under the 500-line file-length gate (Codacy, PR 220 head
//! bae99e4). Like the rest of the panel contracts, these run inside the
//! crate's single GTK-initializing `#[test]` (GTK must be initialized
//! exactly once per process and used from a single thread); see
//! `crate::ui::widget_test_session`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::audio::equalizer::{ClipProtection, EqSettings, Preset};

use super::build::build_equalizer_group;
use super::widget_tests::{
    clip_dropdown, preset_dropdown, redeliver_reflection_notifies, PanelOutput, PanelState,
};
use super::SharedAudioOutput;
/// Settle the parked clip edit the way the engine's main-context poll
/// does (refinery round 3, PR 220): record the adopted outcome exactly
/// as `reconcile_adopted_clip_swap` writes it — a successful edit lands
/// `requested`, a rollback keeps the previously installed truth — then
/// invoke the panel-registered resync when, and only when, the recorded
/// value moved. This is the widget-side half of the real poll path; the
/// audio-side fixtures prove the notification discipline on the engine
/// itself.
fn settle_parked_clip_swap(
    state: &RefCell<PanelState>,
    requested: ClipProtection,
    installed: bool,
) {
    let (moved, resync) = {
        let mut state = state.borrow_mut();
        let previous = state.settings.clip_protection;
        state.settings.clip_protection = if installed { requested } else { previous };
        (
            state.settings.clip_protection != previous,
            state.on_resync.clone(),
        )
    };
    if moved {
        if let Some(resync) = resync {
            resync();
        }
    }
}

/// Shared closer for the resync scenarios: the clip combo shows
/// `clip_position`, no apply happened beyond `applies` (a resync is a
/// display refresh, never an edit), and the named `Pop` preset survives
/// recorded and displayed (no `mark_custom`, no preset flip).
fn assert_resync_left_the_panel_consistent(
    state: &RefCell<PanelState>,
    clip: &gtk::DropDown,
    preset: &gtk::DropDown,
    applies: u32,
    clip_position: u32,
) {
    assert_eq!(
        state.borrow().apply_calls,
        applies,
        "a resync must neither re-apply identical settings nor arm a spurious save"
    );
    assert_eq!(
        state.borrow().settings.preset,
        Preset::Pop,
        "a resync must not move the persisted preset to custom"
    );
    assert_eq!(
        clip.selected(),
        clip_position,
        "the real combo must display the recorded clip policy"
    );
    assert_eq!(preset.selected(), 1, "the named preset must stay displayed");
}

/// Panel fixture shared by the resync scenarios: a supported output
/// recording enabled EQ with the named `Pop` preset and clip protection
/// `Soft` — the combo starts at position 1 (`Soft`), the preset combo at
/// position 1 (`Pop`).
fn resync_scenario_panel() -> (
    SharedAudioOutput,
    Rc<RefCell<PanelState>>,
    adw::PreferencesGroup,
) {
    let (output, state) = PanelOutput::panel(
        true,
        EqSettings {
            enabled: true,
            preset: Preset::Pop,
            preamp_db: Preset::Pop.recommended_preamp_db(),
            bands_db: Preset::Pop.band_gains_db(),
            clip_protection: ClipProtection::Soft,
        },
    );
    let group = build_equalizer_group(&output);
    (output, state, group)
}

/// Refinery corrective round 3 (PR 220): a clip toggle parked across the
/// limiter-edit engagement window leaves the open panel displaying the
/// walked-back `Soft`; when the poll later adopts the parked edit, the
/// panel-registered resync must move the real combo to the adopted
/// policy — without re-applying, without flipping the named preset to
/// `custom`, and with the resync's own reflection echoes staying inert.
fn adopted_clip_swap_resyncs_the_open_panel_display() {
    let (_output, state, group) = resync_scenario_panel();
    let clip = clip_dropdown(&group);
    let preset = preset_dropdown(&group);
    assert_eq!(
        clip.selected(),
        1,
        "the panel starts from the recorded Soft"
    );
    assert!(
        state.borrow().on_resync.is_some(),
        "the panel must register a resync closure"
    );

    // The user's Off toggle parks across the engagement window: the
    // apply refuses it and the applied-state reflection snaps the combo
    // back to the recorded Soft — the walked-back display.
    state.borrow_mut().refuse_clip = true;
    clip.set_selected(0);
    assert_eq!(
        clip.selected(),
        1,
        "the refused toggle must leave the walked-back Soft displayed"
    );
    assert_eq!(state.borrow().apply_calls, 1, "one apply per user edit");

    // The poll adopts the parked edit (installed Off): the recorded
    // value moved, so the engine notifies the panel resync.
    settle_parked_clip_swap(&state, ClipProtection::Off, true);
    assert_resync_left_the_panel_consistent(&state, &clip, &preset, 1, 0);

    // The resync's own reflection can echo late: it must stay inert.
    redeliver_reflection_notifies(&group);
    assert_resync_left_the_panel_consistent(&state, &clip, &preset, 1, 0);
}

/// Echo-safety control of the resync contract (refinery round 3,
/// PR 220): an adoption landing the already-recorded value — a rollback
/// restoring the pre-edit layout — notifies nothing, and even a resync
/// that does run while the recorded value already equals the display
/// must be a pure display refresh: no additional apply, no preset flip,
/// and its own late echoes inert.
fn resync_landing_the_displayed_value_changes_nothing() {
    let (_output, state, group) = resync_scenario_panel();
    let clip = clip_dropdown(&group);
    let preset = preset_dropdown(&group);

    // Park the user's Off toggle: the panel walks back to Soft.
    state.borrow_mut().refuse_clip = true;
    clip.set_selected(0);
    assert_eq!(clip.selected(), 1, "the walked-back Soft is displayed");
    assert_eq!(state.borrow().apply_calls, 1, "one apply per user edit");

    // Rollback: the reconciled value equals the recorded value, so the
    // poll reconciles silently — nothing may change on the panel.
    settle_parked_clip_swap(&state, ClipProtection::Off, false);
    assert_resync_left_the_panel_consistent(&state, &clip, &preset, 1, 1);

    // Control: a resync that does run while the recorded value already
    // equals the display must change nothing.
    let resync = state.borrow().on_resync.clone().expect("registered resync");
    resync();
    assert_resync_left_the_panel_consistent(&state, &clip, &preset, 1, 1);

    // Its own late echoes are inert too.
    redeliver_reflection_notifies(&group);
    assert_resync_left_the_panel_consistent(&state, &clip, &preset, 1, 1);
}

/// Entry point for the round-3 resync contracts: invoked from
/// [`super::widget_tests::equalizer_panel_widget_contracts`] so the
/// crate's single GTK test runs them with the rest of the panel suite.
pub(super) fn resync_widget_contracts() {
    adopted_clip_swap_resyncs_the_open_panel_display();
    resync_landing_the_displayed_value_changes_nothing();
}
