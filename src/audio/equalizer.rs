//! Ten-band equalizer for the local GStreamer output.
//!
//! [`EqualizerSettings`] is the user's state, persisted as the `equalizer`
//! field of `config.json`. [`PlayerEqualizer`] owns the filter bin that the
//! local player installs once as `playbin3`'s `audio-filter`:
//!
//! ```text
//! audioresample ! audioconvert ! capsfilter(F32LE) ! volume (preamp)
//!   ! equalizer-10bands ! rglimiter ! audioconvert ! audioresample
//! ```
//!
//! Every element stays in the bin for the life of the player, and every
//! setting is a plain property write that the elements accept while
//! playing, so edits take effect on the next buffer without relinking.
//! "Disabled" writes neutral values (unity preamp, 0 dB bands, limiter off),
//! at which the three processing elements run in passthrough. Only the
//! sample format is pinned; rate and channel count follow the stream.

use std::cell::RefCell;

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::warn;

/// Centre frequencies of the `equalizer-10bands` bands, in hertz.
pub const BAND_CENTERS_HZ: [u32; 10] = [29, 59, 119, 237, 474, 947, 1889, 3770, 7523, 15011];

/// Lowest band or preamp gain, in dB (the `equalizer-10bands` range).
pub const MIN_GAIN_DB: f64 = -24.0;

/// Highest band or preamp gain, in dB.
pub const MAX_GAIN_DB: f64 = 12.0;

/// Gain resolution of the settings and the sliders, in dB.
pub const GAIN_STEP_DB: f64 = 0.5;

/// A named set of band gains and a matching preamp.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    #[default]
    Flat,
    Pop,
    Rock,
    Jazz,
    Classical,
    /// Gains the user edited by hand.
    Custom,
}

impl Preset {
    /// Every preset, in menu order.
    pub const ALL: [Self; 6] = [
        Self::Flat,
        Self::Pop,
        Self::Rock,
        Self::Jazz,
        Self::Classical,
        Self::Custom,
    ];

    /// The band gains this preset sets, or `None` for [`Preset::Custom`].
    pub const fn band_gains_db(self) -> Option<[f64; 10]> {
        match self {
            Self::Flat => Some([0.0; 10]),
            Self::Pop => Some([1.0, 2.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0]),
            Self::Rock => Some([3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0, 2.0]),
            Self::Jazz => Some([2.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 2.0, 2.0, 1.0]),
            Self::Classical => Some([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]),
            Self::Custom => None,
        }
    }

    /// The preamp that keeps this preset's boosted bands clear of clipping.
    pub const fn preamp_db(self) -> f64 {
        match self {
            Self::Pop | Self::Classical => -2.0,
            Self::Rock | Self::Jazz => -1.0,
            Self::Flat | Self::Custom => 0.0,
        }
    }
}

/// What happens to samples the gains push past full scale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipProtection {
    /// Samples pass unchanged; the output stage may clip them.
    #[default]
    Off,
    /// `rglimiter` compresses peaks above -6 dBFS towards 0 dBFS.
    Soft,
}

/// The persisted equalizer state. The default is the fresh-install state:
/// disabled, Flat, no clip protection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqualizerSettings {
    pub enabled: bool,
    pub preset: Preset,
    pub preamp_db: f64,
    /// Band gains in [`BAND_CENTERS_HZ`] order.
    pub bands_db: [f64; 10],
    pub clip_protection: ClipProtection,
}

impl EqualizerSettings {
    /// Select `preset`, loading its gains unless it is [`Preset::Custom`].
    pub fn select_preset(&mut self, preset: Preset) {
        if let Some(bands) = preset.band_gains_db() {
            self.bands_db = bands;
            self.preamp_db = preset.preamp_db();
        }
        self.preset = preset;
    }

    /// Clamp and snap every gain onto the slider grid, and relabel gains that
    /// no longer match their named preset as [`Preset::Custom`].
    #[must_use]
    pub fn validated(mut self) -> Self {
        self.preamp_db = snap_gain_db(self.preamp_db);
        for gain in &mut self.bands_db {
            *gain = snap_gain_db(*gain);
        }
        #[allow(clippy::float_cmp)] // both sides are exact half-dB steps
        let matches_preset = self.preset.band_gains_db().is_none_or(|bands| {
            bands == self.bands_db && self.preset.preamp_db() == self.preamp_db
        });
        if !matches_preset {
            self.preset = Preset::Custom;
        }
        self
    }

    /// Read the `config.json` field without letting a malformed equalizer
    /// block discard the rest of the configuration: anything unreadable
    /// falls back to the defaults, and everything readable is validated.
    pub fn deserialize_lenient<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(serde_json::from_value::<Self>(value).map_or_else(
            |error| {
                warn!(%error, "Ignoring unreadable equalizer settings");
                Self::default()
            },
            Self::validated,
        ))
    }
}

/// Clamp `db` into the gain range and round it to the nearest half dB.
/// Non-finite input maps to 0 dB.
pub fn snap_gain_db(db: f64) -> f64 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db / GAIN_STEP_DB).round() * GAIN_STEP_DB).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
}

/// Linear amplitude factor for a gain in dB.
fn db_to_factor(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

/// The filter bin and handles to the elements that settings write to.
struct EqualizerBin {
    bin: gst::Bin,
    preamp: gst::Element,
    bands: gst::Element,
    limiter: gst::Element,
}

impl EqualizerBin {
    /// Build the bin with neutral settings.
    ///
    /// # Errors
    /// Fails when an element is not installed (`equalizer-10bands` and
    /// `rglimiter` ship in gst-plugins-good) or the bin cannot be linked.
    fn new() -> Result<Self, glib::BoolError> {
        let make = |factory: &str| gst::ElementFactory::make(factory).build();
        let named =
            |factory: &str, name: &str| gst::ElementFactory::make(factory).name(name).build();
        let format = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .build(),
            )
            .build()?;
        let preamp = named("volume", "preamp")?;
        let bands = named("equalizer-10bands", "bands")?;
        let limiter = named("rglimiter", "limiter")?;
        let chain = [
            make("audioresample")?,
            make("audioconvert")?,
            format,
            preamp.clone(),
            bands.clone(),
            limiter.clone(),
            make("audioconvert")?,
            make("audioresample")?,
        ];

        let bin = gst::Bin::with_name("equalizer");
        bin.add_many(&chain)?;
        gst::Element::link_many(&chain)?;
        for (element, direction) in [(&chain[0], "sink"), (&chain[7], "src")] {
            let pad = element
                .static_pad(direction)
                .ok_or_else(|| glib::bool_error!("equalizer element has no {direction} pad"))?;
            bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;
        }

        let equalizer = Self {
            bin,
            preamp,
            bands,
            limiter,
        };
        equalizer.apply(&EqualizerSettings::default());
        Ok(equalizer)
    }

    /// The element to install as `audio-filter`.
    fn element(&self) -> &gst::Element {
        self.bin.upcast_ref()
    }

    /// Write `settings` to the elements. Safe while the pipeline is playing.
    fn apply(&self, settings: &EqualizerSettings) {
        let (preamp_db, bands_db) = if settings.enabled {
            (settings.preamp_db, settings.bands_db)
        } else {
            (0.0, [0.0; 10])
        };
        self.preamp.set_property("volume", db_to_factor(preamp_db));
        for (index, gain) in bands_db.iter().enumerate() {
            self.bands.set_property(&format!("band{index}"), gain);
        }
        self.limiter.set_property(
            "enabled",
            settings.enabled && settings.clip_protection == ClipProtection::Soft,
        );
    }

    /// Whether `message` was posted by this bin or an element inside it.
    fn posted(&self, message: &gst::MessageRef) -> bool {
        message
            .src()
            .is_some_and(|source| source.has_as_ancestor(&self.bin))
    }
}

/// The local player's equalizer, shared with its bus watch.
///
/// The bin is installed once, while the pipeline is still `NULL`, and
/// survives the `NULL` transition of every later load. If one of its
/// elements posts an error, the player drops it and restarts the stream
/// without an equalizer instead of stopping playback.
pub struct PlayerEqualizer {
    bin: RefCell<Option<EqualizerBin>>,
}

impl PlayerEqualizer {
    /// Build the bin and install it on `playbin`. A missing element leaves
    /// the player without an equalizer; playback is unaffected.
    pub fn install(playbin: &gst::Element) -> Self {
        let bin = match EqualizerBin::new() {
            Ok(bin) => {
                playbin.set_property("audio-filter", bin.element());
                Some(bin)
            }
            Err(error) => {
                warn!(%error, "Equalizer unavailable; playing without it");
                None
            }
        };
        Self {
            bin: RefCell::new(bin),
        }
    }

    pub fn is_available(&self) -> bool {
        self.bin.borrow().is_some()
    }

    pub fn apply(&self, settings: &EqualizerSettings) {
        if let Some(bin) = self.bin.borrow().as_ref() {
            bin.apply(settings);
        }
    }

    /// Handle an error `message` posted from inside the equalizer: remove
    /// the equalizer and restart the current stream without it. Returns the
    /// position to seek back to once the restarted stream has prerolled.
    /// Returns `None`, leaving the error to the caller, when the error came
    /// from elsewhere or the restart failed.
    pub fn recover(
        &self,
        message: &gst::MessageRef,
        playbin: &gst::Element,
    ) -> Option<gst::ClockTime> {
        if !self
            .bin
            .borrow()
            .as_ref()
            .is_some_and(|bin| bin.posted(message))
        {
            return None;
        }
        warn!("Equalizer element failed; continuing playback without the equalizer");
        self.bin.replace(None);
        let position = playbin
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        // As on every load, flush the bus across the teardown so messages the
        // failed stream still has queued are not handled as the restart's.
        let bus = playbin.bus();
        if let Some(bus) = &bus {
            bus.set_flushing(true);
        }
        let _ = playbin.set_state(gst::State::Null);
        playbin.set_property("audio-filter", None::<&gst::Element>);
        if let Some(bus) = &bus {
            bus.set_flushing(false);
        }
        playbin
            .set_state(gst::State::Playing)
            .is_ok()
            .then_some(position)
    }
}

#[cfg(test)]
#[path = "equalizer_tests.rs"]
mod tests;
