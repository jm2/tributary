//! Equalizer contract implementation (docs/equalizer.md, issue #49).
//!
//! This module owns the bounded pieces of the equalizer feature:
//!
//! - [`EqSettings`] — the single typed state struct (enabled, preset,
//!   preamp, ten band gains, clip protection) with the contract's fixed
//!   bounds and fresh-install defaults.
//! - [`config`] — persistence: the six-key `equalizer.cfg` grammar
//!   beside the volume file, with strict validation, boundary clamping,
//!   and an atomic-replace writer (temp file + fsync + rename +
//!   directory fsync).
//! - [`chain`] — [`build_eq_bin`]-equivalent: the local-pipeline
//!   GStreamer filter bin per the *Filter graph* section
//!   (`audioresample ! audioconvert ! capsfilter ! volume !
//!   equalizer-10bands [! rglimiter] ! audioconvert ! audioresample !
//!   capsfilter`) and the buffer-boundary property-write transaction.
//!
//! The capability matrix (which outputs can render equalizer DSP) is
//! reported honestly by each [`AudioOutput`](super::output::AudioOutput)
//! implementation through `supports_equalizer`; this module exposes the
//! shared matrix helper [`output_type_supports_equalizer`].
//!
//! Band centres, preset gain vectors, bounds, defaults, and diagnostics
//! wording are all fixed by the design document. Deviations are contract
//! changes and require a revision there first.

pub mod chain;
pub mod config;

pub use chain::EqChain;
pub use config::{
    config_file_exists, load_settings_with_diagnostic, save_equalizer_settings_to_disk,
};

// ── Contract constants ──────────────────────────────────────────────────

/// Canonical ten band centre frequencies (Hz) of `equalizer-10bands`
/// (gst-plugins-good 1.28.5, verified with `gst-inspect-1.0`).
pub const BAND_CENTERS_HZ: [u32; 10] = [29, 59, 119, 237, 474, 947, 1889, 3770, 7523, 15011];

/// Minimum band/preamp gain in dB (contract bound).
pub const MIN_GAIN_DB: f64 = -24.0;

/// Maximum band/preamp gain in dB (contract bound).
pub const MAX_GAIN_DB: f64 = 12.0;

/// Debounce window for coalescing slider-drag persistence writes.
pub const SAVE_DEBOUNCE_MS: u64 = 750;

// ── Types ───────────────────────────────────────────────────────────────

/// Named equalizer preset. `Custom` is write-only: it is never selectable
/// from the UI and is persisted only as a side-effect of a manual edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Preset {
    #[default]
    Flat,
    Pop,
    Rock,
    Jazz,
    Classical,
    Custom,
}

impl Preset {
    /// Persistence key (always English regardless of UI locale).
    pub fn key(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Pop => "pop",
            Self::Rock => "rock",
            Self::Jazz => "jazz",
            Self::Classical => "classical",
            Self::Custom => "custom",
        }
    }

    /// Parse a persistence key. Unknown/legacy values coerce to `Flat`
    /// per the validation rules (the band vector is not touched).
    pub fn from_key(key: &str) -> Self {
        match key {
            "pop" => Self::Pop,
            "rock" => Self::Rock,
            "jazz" => Self::Jazz,
            "classical" => Self::Classical,
            "custom" => Self::Custom,
            _ => Self::Flat,
        }
    }

    /// Menu position of `Custom` in the fixed six-entry preset combo.
    pub const CUSTOM_MENU_POSITION: usize = 5;

    /// Fixed band gain vector (dB) for this preset — the design appendix
    /// is the source of truth; any deviation is a contract change.
    pub fn band_gains_db(self) -> [f64; 10] {
        match self {
            //                                      29   59  119  237  474  947 1889 3770 7523 15011
            Self::Pop => [1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0],
            Self::Rock => [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0],
            Self::Jazz => [2.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 2.0, 2.0, 1.0],
            Self::Classical => [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0],
            Self::Flat | Self::Custom => [0.0; 10],
        }
    }

    /// Recommended preamp (dB) written together with the band vector.
    pub fn recommended_preamp_db(self) -> f64 {
        match self {
            Self::Pop | Self::Classical => -2.0,
            Self::Rock | Self::Jazz => -1.0,
            Self::Flat | Self::Custom => 0.0,
        }
    }
}

/// Clip-protection policy. `Soft` inserts the fixed `rglimiter` element
/// (soft-knee compressor, −6 dBFS threshold, asymptotic 0 dBFS ceiling).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClipProtection {
    #[default]
    Off,
    Soft,
}

impl ClipProtection {
    pub fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Soft => "soft",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "soft" => Self::Soft,
            _ => Self::Off,
        }
    }
}

/// The complete equalizer state — one typed struct captured per write
/// transaction (contract: *Band and preamp mechanics*).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EqSettings {
    /// Global bypass. `false` means no equalizer bin exists in the
    /// pipeline at all; persisted settings stay on disk untouched.
    pub enabled: bool,
    /// Active preset. `Custom` after any manual band/preamp edit.
    pub preset: Preset,
    /// Global preamp in linear dB, `−24.0 … 0.0 … +12.0`, 0.5 dB steps.
    pub preamp_db: f64,
    /// Ten band gains in linear dB, index 0 = 29 Hz … index 9 = 15011 Hz.
    pub bands_db: [f64; 10],
    /// Clip-protection policy.
    pub clip_protection: ClipProtection,
}

impl EqSettings {
    /// Clamp a gain value into the contract range. Used on read; the UI
    /// must reject out-of-range input at the boundary instead.
    pub fn clamp_gain_db(value: f64) -> f64 {
        value.clamp(MIN_GAIN_DB, MAX_GAIN_DB)
    }

    /// Convert a preamp dB value to the `volume` element's linear factor
    /// (`factor = 10^(dB/20)`); `0.0 dB` maps to unity `1.0`.
    pub fn preamp_db_to_factor(preamp_db: f64) -> f64 {
        10.0_f64.powf(preamp_db / 20.0)
    }

    /// True when this state is byte-for-byte the fresh-install default.
    /// Persistence is suppressed entirely in this state.
    pub fn is_fresh_default(&self) -> bool {
        *self == Self::default()
    }

    /// The manual-edit side effect: any band or preamp change from a
    /// named preset moves the persisted name to `Custom`.
    pub fn mark_custom(&mut self) {
        self.preset = Preset::Custom;
    }
}

// ── Capability matrix ───────────────────────────────────────────────────

/// Capability-matrix row for an output type. Equalizer DSP runs in the
/// local pipeline only; AirPlay, Chromecast, and MPD receivers render
/// audio end-to-end and are `unsupported` under this contract.
///
/// Production reporting flows through the per-implementation
/// `AudioOutput::supports_equalizer` overrides; this shared helper
/// documents and tests the matrix itself.
#[allow(dead_code)]
pub fn output_type_supports_equalizer(output_type: super::output::OutputType) -> bool {
    matches!(output_type, super::output::OutputType::Local)
}

/// Localization key of the closed-form explanation shown by the settings
/// UI for an unsupported active output.
pub fn unsupported_explanation_key(output_type: super::output::OutputType) -> &'static str {
    match output_type {
        super::output::OutputType::Local => "equalizer.unsupported.local",
        super::output::OutputType::AirPlay => "equalizer.unsupported.airplay",
        super::output::OutputType::Chromecast => "equalizer.unsupported.chromecast",
        super::output::OutputType::Mpd => "equalizer.unsupported.mpd",
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests {
    use super::*;

    #[test]
    fn fresh_install_default_matches_the_contract() {
        let defaults = EqSettings::default();
        assert!(!defaults.enabled);
        assert_eq!(defaults.preset, Preset::Flat);
        assert_eq!(defaults.preamp_db, 0.0);
        assert!(defaults.bands_db.iter().all(|gain| *gain == 0.0));
        assert_eq!(defaults.clip_protection, ClipProtection::Off);
        assert!(defaults.is_fresh_default());
    }

    #[test]
    fn band_centers_are_the_canonical_ten() {
        assert_eq!(
            BAND_CENTERS_HZ,
            [29, 59, 119, 237, 474, 947, 1889, 3770, 7523, 15011]
        );
    }

    #[test]
    fn preset_vectors_match_the_design_appendix() {
        assert_eq!(Preset::Flat.band_gains_db(), [0.0; 10]);
        assert_eq!(Preset::Flat.recommended_preamp_db(), 0.0);

        assert_eq!(
            Preset::Pop.band_gains_db(),
            [1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0]
        );
        assert_eq!(Preset::Pop.recommended_preamp_db(), -2.0);

        assert_eq!(
            Preset::Rock.band_gains_db(),
            [3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0]
        );
        assert_eq!(Preset::Rock.recommended_preamp_db(), -1.0);

        assert_eq!(
            Preset::Jazz.band_gains_db(),
            [2.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 2.0, 2.0, 1.0]
        );
        assert_eq!(Preset::Jazz.recommended_preamp_db(), -1.0);

        assert_eq!(
            Preset::Classical.band_gains_db(),
            [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]
        );
        assert_eq!(Preset::Classical.recommended_preamp_db(), -2.0);
    }

    #[test]
    fn preamp_factor_conversion_matches_the_contract_range() {
        assert!((EqSettings::preamp_db_to_factor(0.0) - 1.0).abs() < 1e-12);
        assert!((EqSettings::preamp_db_to_factor(-24.0) - 0.0631).abs() < 1e-4);
        assert!((EqSettings::preamp_db_to_factor(12.0) - 3.9811).abs() < 1e-4);
        // The whole contract range stays inside the volume element's 0..10.
        for step in -48..=24 {
            let db = step as f64 / 2.0;
            let factor = EqSettings::preamp_db_to_factor(db);
            assert!(
                (0.0..=10.0).contains(&factor),
                "{db} dB out of element range"
            );
        }
    }

    #[test]
    fn only_the_local_output_claims_equalizer_support() {
        use super::super::output::OutputType;
        assert!(output_type_supports_equalizer(OutputType::Local));
        assert!(!output_type_supports_equalizer(OutputType::AirPlay));
        assert!(!output_type_supports_equalizer(OutputType::Chromecast));
        assert!(!output_type_supports_equalizer(OutputType::Mpd));
    }

    #[test]
    fn unsupported_explanations_have_a_dedicated_locale_key() {
        use super::super::output::OutputType;
        let keys = [
            unsupported_explanation_key(OutputType::Local),
            unsupported_explanation_key(OutputType::AirPlay),
            unsupported_explanation_key(OutputType::Chromecast),
            unsupported_explanation_key(OutputType::Mpd),
        ];
        assert!(keys.iter().all(|key| key.starts_with("equalizer.")));
    }
}
