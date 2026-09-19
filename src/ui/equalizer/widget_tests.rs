//! Real-widget tests for the equalizer settings panel.
//!
//! GTK must be initialized exactly once per process and used from a
//! single thread, so — like every other GTK-touching contract in this
//! crate — these helpers run inside the crate's single
//! GTK-initializing `#[test]`
//! ([`crate::ui::browser::tests::gtk_widget_contracts_hold_on_one_session`])
//! rather than as their own `#[test]`. See
//! [`crate::ui::widget_test_session`].
//!
//! Coverage:
//! - **F2 (operator review of PR #220):** a deferred enable/disable and a
//!   rejected limiter toggle must leave the actual GTK control showing the
//!   *applied* state, not the refused click, and a later user retry must
//!   apply — with the re-entrancy guard provably suppressing a recursive
//!   callback loop (the apply counter advances exactly once per user edit).
//! - **F4 / r3985258427:** the unsupported-output rendering must attach its
//!   own accessible-description relation to the exposed reset and reload
//!   buttons, not only their parent row.
//! - **Refinery review 2026-09-18T18:41Z:** the echo-safety invariant of
//!   the module header must hold for *every* reflected control, not only
//!   enable/clip: after a Reset or Reload reflection, a late redelivery
//!   of the panel's own value-changed / selected notifies must neither
//!   re-apply (arming a spurious debounced save) nor move the persisted
//!   preset to `custom`, while a genuine differing edit still applies
//!   exactly once.
//! - **Refinery corrective round 3 (PR 220):** a clip toggle parked
//!   across the limiter-edit engagement window leaves the panel showing
//!   the walked-back value; the panel-registered resync must move the
//!   real combo to the adopted policy without re-applying or flipping
//!   the named preset, and an adoption landing the already-displayed
//!   value must change nothing (contracts live in
//!   [`super::widget_resync_tests`]).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::architecture::media::ResolvedHttpRequest;
use crate::audio::equalizer::{ClipProtection, EqSettings, Preset};
use crate::audio::output::{AudioOutput, OutputType};
use crate::audio::{PlayerEventGeneration, PlayerState};
use crate::local::resolver::ResolvedLocalMedia;

use super::build::{build_equalizer_group, described_by_log};
use super::SharedAudioOutput;

/// Mutable state shared between the panel under test and the test body.
pub(super) struct PanelState {
    pub(super) settings: EqSettings,
    /// When set, `apply_equalizer_settings` refuses an `enabled` flip and
    /// retains the installed truth (a deferred install/uninstall).
    refuse_enable: bool,
    /// When set, it refuses a clip-protection flip and retains the
    /// installed truth (a rejected limiter surgery).
    pub(super) refuse_clip: bool,
    /// Number of `apply_equalizer_settings` calls, so the tests can prove
    /// the re-entrancy guard prevents a recursive re-apply.
    pub(super) apply_calls: u32,
    /// The display-resync closure the panel registered (refinery round
    /// 3, PR 220) — single slot, replaced on re-registration, exactly
    /// like the engine's registration seam.
    pub(super) on_resync: Option<Rc<dyn Fn()>>,
}

/// An `AudioOutput` stub that reproduces `Player`'s recorded-vs-installed
/// discipline: a refused enable/clip edit keeps the previously installed
/// value, exactly as `apply_equalizer_settings` walks the recorded state
/// back to the installed topology.
pub(super) struct PanelOutput {
    state: Rc<RefCell<PanelState>>,
    supported: bool,
}

impl PanelOutput {
    pub(super) fn panel(
        supported: bool,
        settings: EqSettings,
    ) -> (SharedAudioOutput, Rc<RefCell<PanelState>>) {
        let state = Rc::new(RefCell::new(PanelState {
            settings,
            refuse_enable: false,
            refuse_clip: false,
            apply_calls: 0,
            on_resync: None,
        }));
        let output: SharedAudioOutput = Rc::new(RefCell::new(Box::new(Self {
            state: Rc::clone(&state),
            supported,
        })));
        (output, state)
    }
}

impl AudioOutput for PanelOutput {
    fn name(&self) -> &str {
        "equalizer-panel-test"
    }

    fn output_type(&self) -> OutputType {
        OutputType::Local
    }

    fn supports_volume(&self) -> bool {
        true
    }

    fn supports_equalizer(&self) -> bool {
        self.supported
    }

    fn equalizer_settings(&self) -> EqSettings {
        self.state.borrow().settings
    }

    fn connect_equalizer_resync(&self, on_resync: Rc<dyn Fn()>) {
        self.state.borrow_mut().on_resync = Some(on_resync);
    }

    fn apply_equalizer_settings(&self, next: EqSettings) {
        let mut state = self.state.borrow_mut();
        state.apply_calls += 1;
        let installed = state.settings;
        let mut applied = next;
        if next.enabled != installed.enabled && state.refuse_enable {
            // Deferred install/uninstall: the installed truth is retained.
            applied.enabled = installed.enabled;
        }
        if applied.enabled && installed.clip_protection != next.clip_protection && state.refuse_clip
        {
            // Deferred/rejected limiter toggle: the installed protection wins.
            applied.clip_protection = installed.clip_protection;
        }
        state.settings = applied;
    }

    fn load_uri(&self, _uri: &str) -> bool {
        false
    }

    fn load_resolved(&self, _request: ResolvedHttpRequest) -> bool {
        false
    }

    fn load_local(&self, _media: ResolvedLocalMedia) -> bool {
        false
    }

    fn set_event_generation(&self, _generation: PlayerEventGeneration) {}

    fn play(&self) {}

    fn pause(&self) {}

    fn stop(&self) {}

    fn toggle_play_pause(&self) {}

    fn seek_to(&self, _position_ms: u64) {}

    fn set_volume(&mut self, _level: f64) {}

    fn volume(&self) -> f64 {
        1.0
    }

    fn state(&self) -> PlayerState {
        PlayerState::Stopped
    }

    fn position_ms(&self) -> Option<u64> {
        None
    }
}

/// Every descendant of `root` (excluding `root` itself) that downcasts
/// to `T`, walking the real widget tree the panel built.
fn descendants<T>(root: &impl IsA<gtk::Widget>) -> Vec<T>
where
    T: IsA<gtk::Widget> + Clone + 'static,
{
    let mut found = Vec::new();
    let mut stack: Vec<gtk::Widget> = root.as_ref().first_child().into_iter().collect();
    while let Some(widget) = stack.pop() {
        if let Ok(matched) = widget.clone().downcast::<T>() {
            found.push(matched);
        }
        if let Some(child) = widget.first_child() {
            stack.push(child);
        }
        if let Some(sibling) = widget.next_sibling() {
            stack.push(sibling);
        }
    }
    found
}

/// The one button in the panel labelled `label`.
fn button_with_label(root: &impl IsA<gtk::Widget>, label: &str) -> gtk::Button {
    descendants::<gtk::Button>(root)
        .into_iter()
        .find(|button| button.label().as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no button labelled {label:?}"))
}

/// The clip-protection combo: the only `DropDown` whose model has the
/// fixed two entries (`0 = Off`, `1 = Soft`).
pub(super) fn clip_dropdown(root: &impl IsA<gtk::Widget>) -> gtk::DropDown {
    descendants::<gtk::DropDown>(root)
        .into_iter()
        .find(|dropdown| dropdown.model().map(|model| model.n_items()).unwrap_or(0) == 2)
        .expect("clip-protection dropdown")
}

/// The preset combo: the only `DropDown` whose model has the fixed six
/// entries (`0 = Flat` .. `4 = Classical`, `5 = Custom`, never
/// activatable).
pub(super) fn preset_dropdown(root: &impl IsA<gtk::Widget>) -> gtk::DropDown {
    descendants::<gtk::DropDown>(root)
        .into_iter()
        .find(|dropdown| dropdown.model().map(|model| model.n_items()).unwrap_or(0) == 6)
        .expect("preset dropdown")
}

/// Redeliver the change notifications GTK would deliver for the panel's
/// own applied-state reflection, after the re-entrancy guard has
/// cleared: every gain scale re-emits `value-changed` with its value
/// unchanged, and the preset combo re-notifies `selected` at its
/// position unchanged. This is the late-echo shape the 2026-09-18
/// review pinned: GTK delivers reflection-driven notifies from inside
/// the triggering handler (suppressed by the guard), but a queued or
/// programmatic redelivery can land once the guard is gone.
pub(super) fn redeliver_reflection_notifies(root: &impl IsA<gtk::Widget>) {
    for scale in descendants::<gtk::Scale>(root) {
        scale.emit_by_name::<()>("value-changed", &[]);
    }
    preset_dropdown(root).notify("selected");
}

/// The enable switch row in the panel.
fn enable_switch(root: &impl IsA<gtk::Widget>) -> adw::SwitchRow {
    descendants::<adw::SwitchRow>(root)
        .into_iter()
        .next()
        .expect("enable switch row")
}

fn translated(key: &str) -> String {
    rust_i18n::t!(key).to_string()
}

/// F2 (enable): a deferred install must snap the switch back to the
/// installed `Off`, not leave it showing the refused `On`; the guard must
/// suppress the recursive re-apply; and a later retry must land `On`.
fn enable_switch_reflects_the_applied_state_and_retries() {
    let (output, state) = PanelOutput::panel(
        true,
        EqSettings {
            enabled: false,
            clip_protection: ClipProtection::Off,
            ..EqSettings::default()
        },
    );
    let group = build_equalizer_group(&output);
    let enable = enable_switch(&group);
    assert!(
        !enable.is_active(),
        "the panel starts from the installed Off"
    );

    // A deferred install: the output refuses the enable.
    state.borrow_mut().refuse_enable = true;
    enable.set_active(true);
    assert!(
        !enable.is_active(),
        "a deferred enable must leave the switch showing the installed Off"
    );
    assert_eq!(
        state.borrow().apply_calls,
        1,
        "the re-entrancy guard must suppress a recursive re-apply"
    );

    // The deferral cleared: a later user retry applies and shows On.
    state.borrow_mut().refuse_enable = false;
    enable.set_active(true);
    assert!(
        enable.is_active(),
        "an applied enable must show On on the real switch"
    );
    assert_eq!(
        state.borrow().apply_calls,
        2,
        "exactly one apply per user edit"
    );
}

/// F2 (clip): a rejected limiter surgery must snap the combo back to the
/// installed `Off`, the guard must suppress recursion, and a later retry
/// must land `Soft`.
fn clip_dropdown_reflects_the_applied_state_and_retries() {
    let (output, state) = PanelOutput::panel(
        true,
        EqSettings {
            enabled: true,
            clip_protection: ClipProtection::Off,
            ..EqSettings::default()
        },
    );
    let group = build_equalizer_group(&output);
    let clip = clip_dropdown(&group);
    assert_eq!(
        clip.selected(),
        0,
        "the panel starts from the installed Off"
    );

    // A rejected limiter toggle: the chain degrades to the no-limiter
    // layout and records `Off`.
    state.borrow_mut().refuse_clip = true;
    clip.set_selected(1);
    assert_eq!(
        clip.selected(),
        0,
        "a rejected limiter toggle must snap the combo back to the installed Off"
    );
    assert_eq!(
        state.borrow().apply_calls,
        1,
        "the re-entrancy guard must suppress a recursive re-apply"
    );

    // The refusal cleared: a later user retry applies and shows Soft.
    state.borrow_mut().refuse_clip = false;
    clip.set_selected(1);
    assert_eq!(
        clip.selected(),
        1,
        "an applied limiter toggle must show Soft on the real combo"
    );
    assert_eq!(
        state.borrow().apply_calls,
        2,
        "exactly one apply per user edit"
    );
}

/// F4 / r3985258427: the unsupported-output rendering must attach the
/// explanation relation to the exposed reset and reload buttons
/// themselves, not only their parent row, and every disabled control
/// must be described.
fn unsupported_panel_describes_every_exposed_control() {
    described_by_log::clear();
    let (output, _state) = PanelOutput::panel(false, EqSettings::default());
    let group = build_equalizer_group(&output);

    let reset = button_with_label(&group, &translated("equalizer.reset_flat"));
    let reload = button_with_label(&group, &translated("equalizer.reload"));
    assert!(
        !reset.is_sensitive(),
        "reset must be insensitive for an unsupported output"
    );
    assert!(
        !reload.is_sensitive(),
        "reload must be insensitive for an unsupported output"
    );
    assert!(
        described_by_log::was_described(reset.as_ptr() as usize),
        "the reset button must carry its own accessible-description relation"
    );
    assert!(
        described_by_log::was_described(reload.as_ptr() as usize),
        "the reload button must carry its own accessible-description relation"
    );

    // Every scale and combo the contract disables is described as well.
    for scale in descendants::<gtk::Scale>(&group) {
        assert!(
            !scale.is_sensitive(),
            "a gain scale must be insensitive for an unsupported output"
        );
        assert!(
            described_by_log::was_described(scale.as_ptr() as usize),
            "every disabled gain scale must be described"
        );
    }
    for dropdown in descendants::<gtk::DropDown>(&group) {
        assert!(
            !dropdown.is_sensitive(),
            "a combo must be insensitive for an unsupported output"
        );
        assert!(
            described_by_log::was_described(dropdown.as_ptr() as usize),
            "every disabled combo must be described"
        );
    }
}

/// Shared echo invariant of the Refinery review 2026-09-18T18:41Z
/// scenarios: after the late redelivery of the panel's own reflection
/// notifies, the recorded state must be untouched — `expected_applies`
/// applies in total (no re-apply, no spurious debounced save), the
/// persisted preset / preamp / band vector still at the restored named
/// values (no `mark_custom`) — and the real combo must still display
/// `expected_position`.
fn assert_named_preset_survives_the_echo(
    state: &RefCell<PanelState>,
    preset: &gtk::DropDown,
    expected_applies: u32,
    expected_preset: Preset,
    expected_preamp_db: f64,
    expected_bands_db: &[f64; 10],
    expected_position: u32,
) {
    assert_eq!(
        state.borrow().apply_calls,
        expected_applies,
        "a late echo must neither re-apply identical settings nor arm a spurious save"
    );
    assert_eq!(
        state.borrow().settings.preset,
        expected_preset,
        "a late echo must not move the persisted preset to custom"
    );
    assert_eq!(state.borrow().settings.preamp_db, expected_preamp_db);
    assert_eq!(
        state.borrow().settings.bands_db,
        *expected_bands_db,
        "a late echo must not alter the restored band vector"
    );
    assert_eq!(
        preset.selected(),
        expected_position,
        "the combo must still show the named preset after the echo"
    );
}

/// Positive control of the reset scenario: a genuine differing edit
/// applies exactly once and moves the persisted preset to `custom`. The
/// dragged scale is whichever gain scale the tree yields first (preamp
/// or band) — either way exactly one gain slot must carry the -2.0 edit
/// and the rest must stay at their restored 0.0.
fn drag_first_gain_scale_and_expect_one_custom_edit(
    state: &RefCell<PanelState>,
    group: &impl IsA<gtk::Widget>,
    preset: &gtk::DropDown,
) {
    let dragged = descendants::<gtk::Scale>(group)
        .into_iter()
        .next()
        .expect("a gain scale to drag");
    dragged.set_value(-2.0);
    assert_eq!(state.borrow().apply_calls, 2, "one apply per real edit");
    let recorded = state.borrow().settings;
    assert_eq!(
        recorded.preset,
        Preset::Custom,
        "a genuine slider edit must move the persisted preset to custom"
    );
    assert_eq!(
        recorded
            .bands_db
            .iter()
            .filter(|gain| **gain == -2.0)
            .count()
            + usize::from(recorded.preamp_db == -2.0),
        1,
        "exactly one gain slot carries the genuine edit"
    );
    assert!(
        recorded
            .bands_db
            .iter()
            .all(|gain| *gain == 0.0 || *gain == -2.0)
            && (recorded.preamp_db == 0.0 || recorded.preamp_db == -2.0),
        "no other gain slot may move"
    );
    assert_eq!(
        preset.selected(),
        5,
        "the combo must display the non-activatable Custom entry"
    );
}

/// Positive control of the reload scenario: a genuine preset choice
/// applies exactly once and records the named preset with its canonical
/// vector.
fn choose_flat_preset_and_expect_one_apply(state: &RefCell<PanelState>, preset: &gtk::DropDown) {
    preset.set_selected(0);
    assert_eq!(
        state.borrow().apply_calls,
        1,
        "one apply per genuine preset choice"
    );
    assert_eq!(
        state.borrow().settings.preset,
        Preset::Flat,
        "the chosen named preset must be recorded"
    );
    assert!(
        state
            .borrow()
            .settings
            .bands_db
            .iter()
            .all(|gain| *gain == 0.0),
        "the chosen preset's canonical vector must be applied"
    );
}

/// Refinery review 2026-09-18T18:41Z (reset): after Reset-to-Flat, a
/// late redelivery of the panel's own reflection notifies must apply
/// nothing (no spurious debounced save), keep the recorded preset at
/// the named `Flat` (no `mark_custom`), and leave the real combo on
/// Flat — while a genuine differing edit still applies exactly once and
/// moves the persisted preset to `custom`.
fn reset_echo_applies_nothing_and_keeps_the_named_preset() {
    let (output, state) = PanelOutput::panel(
        true,
        EqSettings {
            enabled: true,
            preset: Preset::Pop,
            preamp_db: Preset::Pop.recommended_preamp_db(),
            bands_db: Preset::Pop.band_gains_db(),
            clip_protection: ClipProtection::Off,
        },
    );
    let group = build_equalizer_group(&output);
    let reset = button_with_label(&group, &translated("equalizer.reset_flat"));
    let preset = preset_dropdown(&group);
    assert_eq!(
        preset.selected(),
        1,
        "the panel starts from the recorded Pop"
    );

    // Reset to Flat: exactly one apply, then the full reflection (the
    // slider writes and the combo move happen under the guard).
    reset.emit_clicked();
    assert_eq!(state.borrow().apply_calls, 1, "one apply per reset");
    assert_eq!(state.borrow().settings.preset, Preset::Flat);
    assert_eq!(
        preset.selected(),
        0,
        "the reflection must move the real combo to Flat"
    );

    // The late echo: the reflection's own notifies redelivered after
    // the guard cleared. Without the recorded-state skip, every scale
    // echo would re-apply, flip the just-restored Flat to `custom`, and
    // arm a spurious save.
    redeliver_reflection_notifies(&group);
    assert_named_preset_survives_the_echo(&state, &preset, 1, Preset::Flat, 0.0, &[0.0; 10], 0);

    // Positive control: a genuine differing edit applies exactly once
    // and moves the persisted preset to `custom` (helper above).
    drag_first_gain_scale_and_expect_one_custom_edit(&state, &group, &preset);
}

/// Refinery review 2026-09-18T18:41Z (reload): after Reload-from-disk,
/// the late reflection echo must apply nothing and keep both the
/// recorded state and the real combo on the named preset, and a
/// genuine preset choice must still apply exactly once.
fn reload_echo_applies_nothing_and_keeps_the_named_preset() {
    let (output, state) = PanelOutput::panel(
        true,
        EqSettings {
            enabled: true,
            preset: Preset::Jazz,
            preamp_db: Preset::Jazz.recommended_preamp_db(),
            bands_db: Preset::Jazz.band_gains_db(),
            clip_protection: ClipProtection::Off,
        },
    );
    let group = build_equalizer_group(&output);
    let reload = button_with_label(&group, &translated("equalizer.reload"));
    let preset = preset_dropdown(&group);
    assert_eq!(
        preset.selected(),
        3,
        "the panel starts from the recorded Jazz"
    );

    // Reload from disk: the reload path never applies — it re-reads
    // through the output and reflects what the output now reports.
    reload.emit_clicked();
    assert_eq!(
        state.borrow().apply_calls,
        0,
        "reload reflects; it must not arm an apply"
    );
    assert_eq!(state.borrow().settings.preset, Preset::Jazz);

    // The late echo of the reload reflection.
    redeliver_reflection_notifies(&group);
    assert_named_preset_survives_the_echo(
        &state,
        &preset,
        0,
        Preset::Jazz,
        Preset::Jazz.recommended_preamp_db(),
        &Preset::Jazz.band_gains_db(),
        3,
    );

    // Positive control: a genuine preset choice applies exactly once
    // (helper above).
    choose_flat_preset_and_expect_one_apply(&state, &preset);
}

/// Entry point for the crate's single GTK test: runs the equalizer
/// panel's real-widget contracts.
pub fn equalizer_panel_widget_contracts() {
    // The relation recorder is process-wide; start each panel from a
    // clean record.
    described_by_log::clear();
    unsupported_panel_describes_every_exposed_control();
    enable_switch_reflects_the_applied_state_and_retries();
    clip_dropdown_reflects_the_applied_state_and_retries();
    reset_echo_applies_nothing_and_keeps_the_named_preset();
    reload_echo_applies_nothing_and_keeps_the_named_preset();
    super::widget_resync_tests::resync_widget_contracts();
}
