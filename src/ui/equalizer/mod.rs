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
//!
//! Layout: [`build`] constructs the rows and assembles the group,
//! [`wiring`] connects every change handler through the single
//! `apply_equalizer_settings` choke point, and [`widgets`] holds the
//! shared widget and localization helpers.

mod build;
mod widgets;
mod wiring;

#[cfg(test)]
#[allow(clippy::float_cmp)] // snapped gains land on exactly representable half-steps
mod tests;

// Real-widget contracts run inside the crate's single GTK-initializing
// `#[test]` (GTK must be exercised from one thread); see
// `widget_tests` and `crate::ui::widget_test_session`.
#[cfg(all(test, not(target_os = "macos")))]
#[allow(clippy::float_cmp)] // snapped gains land on exactly representable half-steps
pub mod widget_tests;

// Round-3 resync contracts (PR 220), split out of `widget_tests` to
// stay under the file-length gate; same cfg gate, run by the same
// crate-wide GTK test through `widget_tests`.
#[cfg(all(test, not(target_os = "macos")))]
mod widget_resync_tests;

pub use build::build_equalizer_group;

use std::rc::Rc;

use crate::audio::output::AudioOutput;

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
