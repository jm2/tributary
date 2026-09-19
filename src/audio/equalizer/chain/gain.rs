//! Gain delivery for the installed equalizer chain (refinery round 4,
//! PR 220 A2): the snapshot diff that chooses between the documented
//! *direct single-property write* (one changed property) and the
//! idle-pad-probe buffer-boundary batch (two or more), plus the direct
//! writes themselves. Split from `mod.rs` verbatim under the
//! file-length gate; no logic change.

use gst::prelude::*;
use gstreamer as gst;

use super::EqSettings;

/// One adjustable gain property of the installed chain: the preamp
/// stage or one of the ten band gains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqGainProperty {
    /// The preamp `volume` stage (`eq-preamp`).
    Preamp,
    /// One `bandN` gain on `equalizer-10bands`, by index.
    Band(usize),
}

/// The gain properties whose value differs between the two snapshots —
/// the diff that decides between the direct single-property write (one
/// changed property) and the probe-delivered batch (two or more) per the
/// live-reconfiguration boundary.
//
// Exact comparison is the point: gain values live on the fixed 0.5 dB
// grid (`EqSettings::normalize_gain_db`) and cross the boundary
// bit-identically in both directions (UI → engine, file → engine), so
// two snapshots either describe the same gains or different user
// decisions — there is no accumulated drift to tolerate. An
// epsilon-merge here could silently swallow a real 0.5 dB user edit.
#[allow(clippy::float_cmp)]
pub fn changed_gain_properties(previous: &EqSettings, next: &EqSettings) -> Vec<EqGainProperty> {
    let mut changed = Vec::new();
    if previous.preamp_db != next.preamp_db {
        changed.push(EqGainProperty::Preamp);
    }
    for index in 0..previous.bands_db.len() {
        if previous.bands_db[index] != next.bands_db[index] {
            changed.push(EqGainProperty::Band(index));
        }
    }
    changed
}

impl super::EqChain {
    /// Land the gain delta between `previous` and `next` per the
    /// live-reconfiguration boundary (refinery round 4, PR 220 A2):
    ///
    /// - exactly one changed property (one `bandN`, or the preamp alone)
    ///   is a **direct single-property write** — `g_object_set` is atomic
    ///   with respect to the streaming thread, and with one property
    ///   changed there is no intermediate gain combination to bound, so
    ///   the probe transaction would add dispatch latency without
    ///   protecting anything (contract: *Live-reconfiguration boundary*);
    /// - two or more changed properties land as one idle-pad-probe
    ///   buffer-boundary batch ([`Self::apply_band_transaction`]);
    /// - an unchanged gain vector writes nothing.
    ///
    /// Returns whether any property was written.
    pub fn apply_gain_delta(&self, previous: &EqSettings, next: &EqSettings) -> bool {
        match changed_gain_properties(previous, next).as_slice() {
            [] => false,
            [EqGainProperty::Preamp] => {
                self.write_preamp_gain(next.preamp_db);
                true
            }
            [EqGainProperty::Band(index)] => {
                self.write_band_gain(*index, next.bands_db[*index]);
                true
            }
            _ => {
                self.apply_band_transaction(next);
                true
            }
        }
    }

    /// The direct single-property write for the preamp stage: no probe,
    /// no batch (contract: *Live-reconfiguration boundary* — this is the
    /// one update mechanism for a preamp-only edit).
    pub fn write_preamp_gain(&self, preamp_db: f64) {
        self.preamp
            .set_property("volume", EqSettings::preamp_db_to_factor(preamp_db));
    }

    /// The direct single-property write for one band gain: no probe, no
    /// batch, and the other nine band properties are not rewritten
    /// (contract: *Live-reconfiguration boundary*).
    pub fn write_band_gain(&self, index: usize, gain_db: f64) {
        debug_assert!(
            index < 10,
            "equalizer-10bands exposes exactly ten band properties"
        );
        self.eq.set_property(&format!("band{index}"), gain_db);
    }
}
