//! The local-pipeline equalizer filter bin and its live-reconfiguration
//! transactions (contract: *Filter graph* and *Band and preamp
//! mechanics*).

use gst::prelude::*;
use gstreamer as gst;

use super::{ClipProtection, EqSettings};

// ── Bin construction ────────────────────────────────────────────────────

/// Failure reasons for bin construction. All are recoverable: the caller
/// falls back to the existing passthrough layout and keeps going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqBinBuildError {
    /// A required GStreamer element (plugin) is not installed.
    ElementUnavailable(&'static str),
    /// An element could not be added or linked inside the bin.
    ConstructionFailed,
}

impl std::fmt::Display for EqBinBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ElementUnavailable(name) => write!(f, "GStreamer element unavailable: {name}"),
            Self::ConstructionFailed => write!(f, "equalizer bin construction failed"),
        }
    }
}

fn make_element(factory: &'static str, name: &str) -> Result<gst::Element, EqBinBuildError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|_| EqBinBuildError::ElementUnavailable(factory))
}

/// Create the ordered filter-graph elements. The clipper slot exists only
/// when clip protection is on (see `EqChain::build`).
fn make_chain_elements(with_clipper: bool) -> Result<Vec<gst::Element>, EqBinBuildError> {
    let mut specs: Vec<(&'static str, &str)> = vec![
        ("audioresample", "eq-pre-resample"),
        ("audioconvert", "eq-pre-convert"),
        ("capsfilter", "eq-format-pin"),
        ("volume", "eq-preamp"),
        ("equalizer-10bands", "eq"),
    ];
    if with_clipper {
        specs.push(("rglimiter", "clipper"));
    }
    specs.extend([
        ("audioconvert", "eq-post-convert"),
        ("audioresample", "eq-post-resample"),
        ("capsfilter", "eq-sink-pin"),
    ]);
    let elements: Vec<gst::Element> = specs
        .into_iter()
        .map(|(factory, name)| make_element(factory, name))
        .collect::<Result<_, _>>()?;
    if with_clipper {
        grab_element(&elements, "clipper").set_property("enabled", true);
    }
    Ok(elements)
}

/// The pre-EQ capsfilter pins F32LE stereo interleaved; the sample rate
/// stays negotiable so `audioresample` follows the rate `playbin3`
/// negotiates with the decoder.
fn set_format_pin_caps(format_pin: gst::Element) {
    format_pin.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("channels", 2)
            .field("layout", "interleaved")
            .build(),
    );
}

/// Look up one of the just-created chain elements by its unique name.
fn grab_element(elements: &[gst::Element], name: &str) -> gst::Element {
    elements
        .iter()
        .find(|element| element.name() == name)
        .cloned()
        .expect("chain element present")
}

/// Remove a partial layout again so the bin is left empty.
fn rollback_elements(bin: &gst::Bin, elements: &[gst::Element]) {
    for element in elements {
        let _ = bin.remove(element);
    }
}

/// Return one limiter element to NULL and remove it from the bin.
/// `gst_bin_remove` requires a NULL-state child.
fn drop_limiter_from_bin(bin: &gst::Bin, clipper: &gst::Element) {
    let _ = clipper.set_state(gst::State::Null);
    let _ = bin.remove(clipper);
}

// ── Installed chain ─────────────────────────────────────────────────────

/// Handles into one installed equalizer bin. Retained by the local
/// player for the whole time the bin is linked into `playbin3`.
pub struct EqChain {
    /// The complete filter bin installed at `playbin3.audio-filter`.
    pub bin: gst::Bin,
    /// `volume` preamp stage (`eq-preamp`).
    preamp: gst::Element,
    /// `equalizer-10bands` stage (`eq`).
    eq: gst::Element,
    /// Post-EQ `audioconvert` — relink target for limiter surgery.
    post_convert: gst::Element,
    /// `rglimiter` stage (`clipper`), present iff clip protection is on.
    clipper: Option<gst::Element>,
}

impl EqChain {
    /// Build the filter bin per the *Filter graph* section for `settings`.
    ///
    /// The pre-EQ `capsfilter` pins
    /// `audio/x-raw,format=F32LE,channels=2,layout=interleaved`; the sample
    /// rate is deliberately left negotiable so `audioresample` follows the
    /// rate `playbin3` negotiates with the decoder instead of pinning a
    /// rate we cannot know at construction time. The post-EQ `capsfilter`
    /// is created with its caps unset for the same reason: the surrounding
    /// `audioconvert`/`audioresample` adapt to whatever the audio sink
    /// negotiates. If the upstream decoder cannot deliver the pinned caps,
    /// negotiation fails at the state transition, `playbin3` posts the
    /// error to the bus, and the caller rolls the bin back to passthrough.
    pub fn build(settings: &EqSettings) -> Result<Self, EqBinBuildError> {
        let bin = gst::Bin::with_name("eq-bin");
        let with_clipper = settings.clip_protection == ClipProtection::Soft;
        let elements = make_chain_elements(with_clipper)?;
        set_format_pin_caps(grab_element(&elements, "eq-format-pin"));
        Self::install(&bin, &elements)?;

        let chain = Self {
            bin,
            preamp: grab_element(&elements, "eq-preamp"),
            eq: grab_element(&elements, "eq"),
            post_convert: grab_element(&elements, "eq-post-convert"),
            clipper: elements
                .iter()
                .find(|element| element.name() == "clipper")
                .cloned(),
        };
        chain.apply_band_transaction(settings);
        Ok(chain)
    }

    /// Add, link, and expose `elements` on `bin`. On any failure the
    /// partial layout is removed again so the bin is left empty and the
    /// caller falls back to passthrough.
    fn install(bin: &gst::Bin, elements: &[gst::Element]) -> Result<(), EqBinBuildError> {
        let outcome = Self::add_and_link_and_expose(bin, elements);
        if outcome.is_err() {
            rollback_elements(bin, elements);
        }
        outcome
    }

    fn add_and_link_and_expose(
        bin: &gst::Bin,
        elements: &[gst::Element],
    ) -> Result<(), EqBinBuildError> {
        if bin.add_many(elements).is_err() {
            return Err(EqBinBuildError::ConstructionFailed);
        }
        if gst::Element::link_many(elements).is_err() {
            return Err(EqBinBuildError::ConstructionFailed);
        }
        let sink_ghost = Self::ghost_pad(&elements[0], "sink", "audio-filter-sink")?;
        let src_ghost = Self::ghost_pad(
            elements.last().expect("non-empty"),
            "src",
            "audio-filter-src",
        )?;
        if bin.add_pad(&sink_ghost).is_err() || bin.add_pad(&src_ghost).is_err() {
            return Err(EqBinBuildError::ConstructionFailed);
        }
        Ok(())
    }

    /// Build one directional ghost pad over the bin's edge element.
    fn ghost_pad(
        element: &gst::Element,
        direction: &str,
        name: &str,
    ) -> Result<gst::GhostPad, EqBinBuildError> {
        let target = element
            .static_pad(direction)
            .ok_or(EqBinBuildError::ConstructionFailed)?;
        Ok(gst::GhostPad::builder_with_target(&target)
            .map_err(|_| EqBinBuildError::ConstructionFailed)?
            .name(name)
            .build())
    }

    /// Buffer-boundary property-write transaction: capture the full
    /// `EqSettings` into one typed write, wrap the ten band writes and
    /// the preamp write in a notification freeze (RAII guard thawing on
    /// drop) so the bus sees exactly one `properties-changed` per
    /// element, not eleven.
    pub fn apply_band_transaction(&self, settings: &EqSettings) {
        {
            // The freeze guard thaws notifications when dropped.
            let _frozen = self.preamp.freeze_notify();
            self.preamp.set_property(
                "volume",
                EqSettings::preamp_db_to_factor(settings.preamp_db),
            );
        }
        {
            let _frozen = self.eq.freeze_notify();
            for (index, gain) in settings.bands_db.iter().enumerate() {
                // `equalizer-10bands` band properties are gdouble.
                self.eq.set_property(&format!("band{index}"), *gain);
            }
        }
    }

    /// Insert or remove the `rglimiter` element inside the installed bin
    /// (clip-protection toggle). The caller owns the pause/resume seam.
    /// Returns `false` when the surgery failed and the chain degraded to
    /// the no-limiter layout (recoverable per the contract).
    pub fn set_clip_protection(&mut self, soft: ClipProtection) -> bool {
        match (soft, self.clipper.take()) {
            (ClipProtection::Soft, None) => self.insert_limiter(),
            (ClipProtection::Off, Some(clipper)) => self.remove_limiter(&clipper),
            (ClipProtection::Off, None) => true,
            (ClipProtection::Soft, Some(clipper)) => {
                // Already installed: the `take()` above must be undone so
                // the stored handle keeps matching the element linked in
                // the bin (`clip_protection_installed` stays truthful).
                // Re-sync the state first so a live pipeline can never
                // keep the limiter stranded out of step with its bin.
                let _ = clipper.sync_state_with_parent();
                self.clipper = Some(clipper);
                true
            }
        }
    }

    /// Insert `rglimiter` between the EQ stage and the post-convert
    /// stage, then state-sync it with the bin (contract:
    /// *Live-reconfiguration boundary* — on add: link it, then
    /// `gst_element_sync_state_with_parent` so the element's state
    /// follows the running bin). On any failure, degrade to the
    /// no-limiter layout and report `false`.
    fn insert_limiter(&mut self) -> bool {
        let Ok(clipper) = make_element("rglimiter", "clipper") else {
            return false;
        };
        clipper.set_property("enabled", true);
        if self.bin.add(&clipper).is_err() {
            return false;
        }
        // The EQ stage already feeds the post-convert stage directly;
        // break that link to make room for the limiter.
        let was_linked = self
            .eq
            .static_pad("src")
            .map(|src| src.peer().is_some())
            .unwrap_or(false);
        if was_linked {
            // `Element::unlink` returns `()`.
            self.eq.unlink(&self.post_convert);
        }
        if self.eq.link(&clipper).is_ok()
            && clipper.link(&self.post_convert).is_ok()
            && clipper.sync_state_with_parent().is_ok()
        {
            self.clipper = Some(clipper);
            return true;
        }
        // Degrade to the no-limiter layout: restore the direct
        // eq → post-convert link.
        self.eq.unlink(&clipper);
        clipper.unlink(&self.post_convert);
        drop_limiter_from_bin(&self.bin, &clipper);
        let _ = self.eq.link(&self.post_convert);
        false
    }

    /// Remove the installed `rglimiter` and restore the direct
    /// eq → post-convert link.
    fn remove_limiter(&self, clipper: &gst::Element) -> bool {
        self.eq.unlink(clipper);
        drop_limiter_from_bin(&self.bin, clipper);
        self.eq.link(&self.post_convert).is_ok()
    }

    /// True when the `rglimiter` element is currently inside the bin.
    #[allow(dead_code)] // inspection helper; exercised by the contract tests
    pub fn clip_protection_installed(&self) -> bool {
        self.clipper.is_some()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests {
    use std::sync::OnceLock;

    use super::super::Preset;
    use super::*;

    /// Reports whether the host provides the plugins the EQ bin needs,
    /// loading them exactly once per process, single-threaded.
    ///
    /// GStreamer loads a plugin's `.so` lazily on first use, and that load
    /// runs the plugin's `plugin_init`, which registers its GObject types.
    /// The parallel test harness must therefore keep this first touch off
    /// the fast path: four threads racing `plugin_init` on a cold registry
    /// hit duplicate type registration ("cannot register existing type
    /// 'GstIirEqualizerBand'") and segfault. `OnceLock` funnels the whole
    /// load into one initializer while every other thread blocks; after it
    /// completes, element creation is ordinary thread-safe registry access.
    /// Minimal development hosts may omit gst-plugins-good entirely, in
    /// which case this reports `false` and the bin tests skip. Packaged
    /// builds require the plugins, and CI's package jobs exercise that
    /// contract.
    fn bin_requires_plugins() -> bool {
        static EQ_BIN_PLUGINS: OnceLock<bool> = OnceLock::new();
        *EQ_BIN_PLUGINS.get_or_init(|| {
            gst::init().is_ok()
                && gst::ElementFactory::make("equalizer-10bands")
                    .build()
                    .is_ok()
                && gst::ElementFactory::make("rglimiter").build().is_ok()
        })
    }

    /// The pad's peer must live inside the element named `expected_parent`.
    fn assert_peer_element_name(pad: &gst::Pad, expected_parent: &str) {
        let peer_parent = pad
            .peer()
            .expect("pad linked")
            .parent()
            .expect("peer parented");
        assert_eq!(peer_parent.name().as_str(), expected_parent);
    }

    /// The pre-EQ capsfilter pins F32LE stereo interleaved.
    fn assert_pinned_format_caps(bin: &gst::Bin) {
        let format_pin = bin
            .by_name("eq-format-pin")
            .expect("pre-EQ capsfilter present");
        let caps = format_pin.property::<gst::Caps>("caps");
        let structure = caps.structure(0).expect("caps structure");
        assert_eq!(structure.name().as_str(), "audio/x-raw");
        assert_eq!(structure.get::<String>("format").as_deref(), Ok("F32LE"));
        assert_eq!(structure.get::<i32>("channels"), Ok(2));
        assert_eq!(
            structure.get::<String>("layout").as_deref(),
            Ok("interleaved")
        );
    }

    /// The preset transaction reached the volume and equalizer elements.
    fn assert_preset_transaction_reached_elements(bin: &gst::Bin, preamp_db: f64) {
        let preamp = bin.by_name("eq-preamp").expect("preamp present");
        let expected_factor = EqSettings::preamp_db_to_factor(preamp_db);
        // The volume element quantizes its gain to f32 internally even
        // though the property is declared gdouble, so compare at f32
        // precision.
        assert!((preamp.property::<f64>("volume") - expected_factor).abs() < 1e-6);
        let eq = bin.by_name("eq").expect("equalizer present");
        assert!((eq.property::<f64>("band0") - 1.0).abs() < 1e-9);
        assert!((eq.property::<f64>("band2") - 3.0).abs() < 1e-9);
        assert!((eq.property::<f64>("band5") - (-1.0)).abs() < 1e-9);
    }

    /// Chain order: eq → clipper → post-convert, limiter enabled.
    fn assert_links_eq_through_clipper(bin: &gst::Bin) {
        let clipper = bin.by_name("clipper").expect("limiter present");
        assert!(clipper.property::<bool>("enabled"));
        let eq = bin.by_name("eq").expect("equalizer present");
        assert_peer_element_name(&eq.static_pad("src").unwrap(), clipper.name().as_str());
        let post_convert = bin
            .by_name("eq-post-convert")
            .expect("post-convert present");
        assert_peer_element_name(
            &clipper.static_pad("src").unwrap(),
            post_convert.name().as_str(),
        );
    }

    /// eq links directly to the post-convert stage (no limiter).
    fn assert_links_eq_directly_to_post_convert(bin: &gst::Bin) {
        let eq = bin.by_name("eq").expect("equalizer present");
        let post_convert = bin
            .by_name("eq-post-convert")
            .expect("post-convert present");
        assert_peer_element_name(&eq.static_pad("src").unwrap(), post_convert.name().as_str());
    }

    #[test]
    fn eq_bin_layout_matches_the_filter_graph_with_limiter() {
        if !bin_requires_plugins() {
            // Minimal development hosts may omit gst-plugins-good. Packaged
            // builds require it, and CI's package jobs exercise that contract.
            return;
        }
        let settings = EqSettings {
            enabled: true,
            preset: Preset::Pop,
            preamp_db: -2.0,
            bands_db: Preset::Pop.band_gains_db(),
            clip_protection: ClipProtection::Soft,
        };
        let chain = EqChain::build(&settings).expect("eq-bin builds");
        assert_eq!(chain.bin.name(), "eq-bin");
        assert!(chain.bin.static_pad("audio-filter-sink").is_some());
        assert!(chain.bin.static_pad("audio-filter-src").is_some());
        assert!(chain.clip_protection_installed());
        assert_pinned_format_caps(&chain.bin);
        assert_preset_transaction_reached_elements(&chain.bin, -2.0);
        assert_links_eq_through_clipper(&chain.bin);
    }

    #[test]
    fn eq_bin_omits_the_limiter_when_clip_protection_is_off() {
        if !bin_requires_plugins() {
            return;
        }
        let settings = EqSettings {
            enabled: true,
            ..EqSettings::default()
        };
        let chain = EqChain::build(&settings).expect("eq-bin builds");
        assert!(!chain.clip_protection_installed());
        assert!(chain.bin.by_name("clipper").is_none());
        assert_links_eq_directly_to_post_convert(&chain.bin);
    }

    #[test]
    fn limiter_surgery_inserts_and_removes_inside_the_installed_bin() {
        if !bin_requires_plugins() {
            return;
        }
        let mut chain = EqChain::build(&EqSettings {
            enabled: true,
            ..EqSettings::default()
        })
        .expect("eq-bin builds");

        // Off → Soft: insert.
        assert!(chain.set_clip_protection(ClipProtection::Soft));
        assert!(chain.clip_protection_installed());
        let clipper = chain.bin.by_name("clipper").expect("inserted limiter");
        assert!(clipper.property::<bool>("enabled"));
        let eq = chain.bin.by_name("eq").unwrap();
        assert_peer_element_name(&eq.static_pad("src").unwrap(), clipper.name().as_str());

        // Soft → Off: remove.
        assert!(chain.set_clip_protection(ClipProtection::Off));
        assert!(!chain.clip_protection_installed());
        assert!(chain.bin.by_name("clipper").is_none());
        assert_links_eq_directly_to_post_convert(&chain.bin);
    }

    /// Regression (contract acceptance 6, live-pipeline half): dynamic
    /// `rglimiter` insertion must state-sync the new element with its
    /// parent bin. A bin brought to `READY` completes that transition
    /// synchronously, so a limiter inserted afterwards starts in `NULL`
    /// unless the insert calls `gst_element_sync_state_with_parent` —
    /// the exact live-pipeline hazard this regression pins.
    #[test]
    fn dynamic_limiter_insertion_syncs_state_with_the_running_bin() {
        if !bin_requires_plugins() {
            return;
        }
        let mut chain = EqChain::build(&EqSettings {
            enabled: true,
            ..EqSettings::default()
        })
        .expect("eq-bin builds");
        chain
            .bin
            .set_state(gst::State::Ready)
            .expect("a no-data bin reaches READY synchronously");
        assert_eq!(chain.bin.current_state(), gst::State::Ready);

        assert!(chain.set_clip_protection(ClipProtection::Soft));
        let clipper = chain.bin.by_name("clipper").expect("inserted limiter");
        assert_eq!(
            clipper.current_state(),
            chain.bin.current_state(),
            "the inserted limiter must follow its bin's state, not stay in NULL"
        );

        // Removal returns the limiter to NULL before it leaves the bin.
        assert!(chain.set_clip_protection(ClipProtection::Off));
        assert!(chain.bin.by_name("clipper").is_none());
        chain.bin.set_state(gst::State::Null).expect("bin to NULL");
    }

    #[test]
    fn band_transaction_updates_a_live_chain_in_one_write_set() {
        if !bin_requires_plugins() {
            return;
        }
        let chain = EqChain::build(&EqSettings {
            enabled: true,
            ..EqSettings::default()
        })
        .expect("eq-bin builds");

        let next = EqSettings {
            enabled: true,
            preset: Preset::Custom,
            preamp_db: 12.0,
            bands_db: [-24.0, -12.0, -6.0, -0.5, 0.0, 0.5, 6.0, 12.0, 3.5, 1.5],
            clip_protection: ClipProtection::Off,
        };
        chain.apply_band_transaction(&next);

        let preamp = chain.bin.by_name("eq-preamp").unwrap();
        assert!(
            (preamp.property::<f64>("volume") - EqSettings::preamp_db_to_factor(12.0)).abs() < 1e-6
        );
        let eq = chain.bin.by_name("eq").unwrap();
        for (index, expected) in next.bands_db.iter().enumerate() {
            let written: f64 = eq.property(&format!("band{index}"));
            assert!((written - *expected).abs() < 1e-9, "band{index}");
        }
    }
}
