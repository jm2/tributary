//! The local-pipeline equalizer filter bin and its live-reconfiguration
//! transactions (contract: *Filter graph* and *Band and preamp
//! mechanics*).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use super::{ClipProtection, EqSettings};

/// How long the dynamic limiter edit waits for its blocking pad probe to
/// engage before deferring to the caller's pause/relink fallback. Bounded
/// so a pad that never reports idle can never stall the UI thread.
const LIMITER_PROBE_ENGAGE_TIMEOUT: Duration = Duration::from_millis(200);

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

/// Test-only deterministic fault injection for the limiter-removal
/// surgery. The variants model the two failure shapes the production
/// code path must survive: the direct relink is refused once, or both
/// the direct relink and the limiter-path restoration are refused. Each
/// fault is consumed when it fires, so a subsequent call retries against
/// the real graph — exactly the retry the caller's recorded-state
/// discipline depends on.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimiterRemoveFault {
    /// The direct `eq → post-convert` relink is refused; the
    /// limiter-path restoration then succeeds.
    DirectRelink,
    /// Both the direct relink and the limiter-path restoration fail.
    DirectRelinkAndRestore,
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
    /// Test-only armed surgery fault; production builds never carry it.
    #[cfg(test)]
    remove_fault: Option<LimiterRemoveFault>,
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
            #[cfg(test)]
            remove_fault: None,
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
    /// `EqSettings` into one typed write and land the ten band writes
    /// and the preamp write as a single batch (contract:
    /// *Live-reconfiguration boundary*).
    ///
    /// While the bin runs, the batch is delivered through an **idle pad
    /// probe** on the bin's sink ghost pad — the mechanism GStreamer
    /// provides for running code between buffers — so no buffer crosses
    /// the bin boundary while the eleven writes are in flight. The
    /// probe uninstalls itself (`PadProbeReturn::Remove`) once the batch
    /// has landed. When the bin is not running (build time, behind the
    /// pause seam, or at READY with no data flow) the writes land
    /// directly: there is no streaming thread to serialize against.
    ///
    /// GObject notify freezing is deliberately not used: freezing only
    /// defers `notify` emission and carries no cross-element atomicity,
    /// so it neither bounds nor batches the audio effect of the writes.
    pub fn apply_band_transaction(&self, settings: &EqSettings) {
        let preamp_factor = EqSettings::preamp_db_to_factor(settings.preamp_db);
        let bands = settings.bands_db;
        let running = self.bin.current_state() != gst::State::Null;
        let sink_pad = self.bin.static_pad("audio-filter-sink");
        match (running, sink_pad) {
            (true, Some(sink_pad)) => {
                let preamp = self.preamp.clone();
                let eq = self.eq.clone();
                sink_pad.add_probe(gst::PadProbeType::IDLE, move |_, _| {
                    Self::write_band_properties_on(&preamp, &eq, preamp_factor, &bands);
                    // The transaction is one-shot: uninstall the probe
                    // so the next batch gets a fresh boundary write.
                    gst::PadProbeReturn::Remove
                });
            }
            _ => Self::write_band_properties_on(&self.preamp, &self.eq, preamp_factor, &bands),
        }
    }

    /// The raw batch: one preamp write, then the ten band writes.
    fn write_band_properties_on(
        preamp: &gst::Element,
        eq: &gst::Element,
        preamp_factor: f64,
        bands: &[f64; 10],
    ) {
        preamp.set_property("volume", preamp_factor);
        for (index, gain) in bands.iter().enumerate() {
            // `equalizer-10bands` band properties are gdouble.
            eq.set_property(&format!("band{index}"), *gain);
        }
    }

    /// Insert or remove the `rglimiter` element inside the installed bin
    /// (clip-protection toggle) as a direct synchronous edit. The caller
    /// owns whichever flow-stopping mechanism guards the edit: the dynamic
    /// blocking pad probe ([`Self::swap_clip_protection_under_block_probe`])
    /// or, as a fallback after a failed dynamic re-link, the pause/relink
    /// seam. Returns `false` when the surgery failed and the chain degraded
    /// to the no-limiter layout (recoverable per the contract).
    pub fn set_clip_protection(&mut self, soft: ClipProtection) -> bool {
        let graph = self.limiter_graph();
        // Precompute the test-fault decisions in the order the surgery
        // consumes them: the direct relink is always attempted, the
        // restoration only after the direct relink is refused.
        let direct_blocked = self.fault_blocks_direct_relink();
        let restore_blocked = self.fault_blocks_restore();
        edit_limiter_topology(
            &graph,
            soft,
            &mut self.clipper,
            direct_blocked,
            restore_blocked,
        )
    }

    /// The element handles one in-bin limiter edit rewires. Cheap clones of
    /// the running graph, so the edit can also run from the blocking probe
    /// callback on the streaming thread without borrowing the chain.
    fn limiter_graph(&self) -> LimiterGraph {
        LimiterGraph {
            bin: self.bin.clone(),
            eq: self.eq.clone(),
            post_convert: self.post_convert.clone(),
        }
    }

    /// Perform the limiter insert/remove as the documented dynamic in-bin
    /// topology edit, inside a blocking pad probe on the
    /// `equalizer-10bands` src pad (contract:
    /// *Live-reconfiguration boundary* — `Clip protection`).
    ///
    /// `GST_PAD_PROBE_TYPE_BLOCK_DOWNSTREAM | GST_PAD_PROBE_TYPE_IDLE`
    /// stops data flow at the EQ output — the boundary every rewired pad
    /// sits behind — before the graph is touched, so no buffer can reach
    /// the unlinked pads and `GST_FLOW_NOT_LINKED` cannot reach the bus
    /// from this path. The probe callback performs the whole edit and
    /// returns `GST_PAD_PROBE_REMOVE` only over a validated topology: the
    /// new layout on success, or the pre-edit layout the surgery restores
    /// on a failed re-link, so blocked flow resumes across a valid chain.
    /// A pipeline that never reports the pad idle within the bounded
    /// window is left untouched and the caller falls back to the
    /// pause/relink seam.
    ///
    /// Returns `Some(true)` when the requested toggle is installed,
    /// `Some(false)` when the dynamic re-link failed and the pre-edit
    /// layout was restored (the caller retries via the pause/relink
    /// seam), and `None` when the probe could not engage or no EQ src pad
    /// exists.
    pub fn swap_clip_protection_under_block_probe(&mut self, soft: ClipProtection) -> Option<bool> {
        let eq_src = self.eq.static_pad("src")?;
        let graph = self.limiter_graph();
        // The probe callback runs on the streaming thread, so it cannot
        // borrow the chain: the owned `rglimiter` handle is threaded
        // through a shared slot and the result is reported over a channel.
        let slot = Arc::new(Mutex::new(self.clipper.take()));
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let signal = Mutex::new(tx);
        let slot_cb = Arc::clone(&slot);
        let probe_id = eq_src.add_probe(
            gst::PadProbeType::BLOCK_DOWNSTREAM | gst::PadProbeType::IDLE,
            move |_pad, _info| {
                let installed = {
                    let mut current = slot_cb
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    edit_limiter_topology(&graph, soft, &mut current, false, false)
                };
                if let Ok(tx) = signal.lock() {
                    let _ = tx.try_send(installed);
                }
                // Uninstall the probe only now, over the validated
                // topology, so blocked data flow resumes across a valid
                // chain (and the caller learns the outcome).
                gst::PadProbeReturn::Remove
            },
        );
        // A pad that reports idle synchronously runs the callback before
        // `add_probe` returns and reports no id; either way the outcome
        // arrives over the channel. A missing outcome means the probe
        // could not engage (or the pad never went idle), so the graph is
        // left untouched for the caller's pause/relink fallback.
        let installed = rx.recv_timeout(LIMITER_PROBE_ENGAGE_TIMEOUT).ok();
        if let Some(probe_id) = probe_id {
            // Harmless when the callback already uninstalled itself.
            eq_src.remove_probe(probe_id);
        }
        // Adopt the handle the callback left in the slot (or the original
        // handle when the callback never ran).
        let retained = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.clipper = retained;
        installed
    }

    /// Test-only: whether the armed fault refuses the direct relink.
    /// Fires once (the fault is consumed), so a later call retries the
    /// real graph. Always `false` in production builds.
    // The production body never reads the (test-only) fault field, so the
    // receiver is genuinely unused there; allow the pedantic lints rather
    // than split the method across cfg signatures.
    #[cfg_attr(
        not(test),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    fn fault_blocks_direct_relink(&mut self) -> bool {
        #[cfg(test)]
        {
            match self.remove_fault {
                Some(LimiterRemoveFault::DirectRelink) => {
                    self.remove_fault = None;
                    true
                }
                // Consumed by the restoration step below.
                Some(LimiterRemoveFault::DirectRelinkAndRestore) => true,
                None => false,
            }
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// Test-only: whether the armed fault refuses the limiter-path
    /// restoration. Fires once. Always `false` in production builds.
    #[cfg_attr(
        not(test),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    fn fault_blocks_restore(&mut self) -> bool {
        #[cfg(test)]
        {
            if self.remove_fault == Some(LimiterRemoveFault::DirectRelinkAndRestore) {
                self.remove_fault = None;
                true
            } else {
                false
            }
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// Test-only seam: arm a deterministic limiter-removal fault at the
    /// real surgery boundary.
    #[cfg(test)]
    pub(crate) fn inject_limiter_remove_fault(&mut self, fault: LimiterRemoveFault) {
        self.remove_fault = Some(fault);
    }

    /// True when the `rglimiter` element is currently inside the bin.
    #[allow(dead_code)] // inspection helper; exercised by the contract tests
    pub fn clip_protection_installed(&self) -> bool {
        self.clipper.is_some()
    }
}

// ── Limiter surgery ─────────────────────────────────────────────────────

/// The element handles one in-bin limiter edit rewires. Cheap clones of the
/// running graph, so the edit can run from the blocking probe callback on
/// the streaming thread without borrowing the chain.
#[derive(Clone)]
struct LimiterGraph {
    bin: gst::Bin,
    eq: gst::Element,
    post_convert: gst::Element,
}

/// Apply one limiter topology edit to `clipper`, the chain's owned
/// `rglimiter` handle (`None` when clip protection is off). The handle is
/// left matching the routed graph, so `clip_protection_installed` stays
/// truthful. Returns whether the requested toggle is installed.
///
/// `direct_blocked`/`restore_blocked` are the test-only fault decisions for
/// the removal surgery; production callers pass `false`.
fn edit_limiter_topology(
    graph: &LimiterGraph,
    soft: ClipProtection,
    clipper: &mut Option<gst::Element>,
    direct_blocked: bool,
    restore_blocked: bool,
) -> bool {
    match (soft, clipper.take()) {
        (ClipProtection::Soft, None) => match insert_limiter(graph) {
            Ok(installed) => {
                *clipper = Some(installed);
                true
            }
            Err(()) => false,
        },
        (ClipProtection::Off, Some(installed)) => {
            let (landed, retained) =
                remove_limiter(graph, installed, direct_blocked, restore_blocked);
            *clipper = retained;
            landed
        }
        (ClipProtection::Off, None) => true,
        (ClipProtection::Soft, Some(installed)) => {
            // Already installed: re-sync the state so a live pipeline can
            // never keep the limiter stranded out of step with its bin.
            let _ = installed.sync_state_with_parent();
            *clipper = Some(installed);
            true
        }
    }
}

/// Insert `rglimiter` between the EQ stage and the post-convert stage, then
/// state-sync it with the bin (contract: *Live-reconfiguration boundary* —
/// on add: link it, then `gst_element_sync_state_with_parent` so the
/// element's state follows the running bin). On any failure, degrade to the
/// no-limiter layout and report `Err`.
fn insert_limiter(graph: &LimiterGraph) -> Result<gst::Element, ()> {
    let Ok(clipper) = make_element("rglimiter", "clipper") else {
        return Err(());
    };
    clipper.set_property("enabled", true);
    if graph.bin.add(&clipper).is_err() {
        return Err(());
    }
    // The EQ stage already feeds the post-convert stage directly; break
    // that link to make room for the limiter.
    let was_linked = graph
        .eq
        .static_pad("src")
        .map(|src| src.peer().is_some())
        .unwrap_or(false);
    if was_linked {
        // `Element::unlink` returns `()`.
        graph.eq.unlink(&graph.post_convert);
    }
    if graph.eq.link(&clipper).is_ok()
        && clipper.link(&graph.post_convert).is_ok()
        && clipper.sync_state_with_parent().is_ok()
    {
        return Ok(clipper);
    }
    // Degrade to the no-limiter layout: restore the direct
    // eq → post-convert link.
    graph.eq.unlink(&clipper);
    clipper.unlink(&graph.post_convert);
    drop_limiter_from_bin(&graph.bin, &clipper);
    let _ = graph.eq.link(&graph.post_convert);
    Err(())
}

/// Remove the installed `rglimiter` and restore the direct
/// eq → post-convert link, returning the owned handle whenever the limiter
/// stays routed.
///
/// Both old links are unlinked **before** the direct relink is attempted
/// (the `post-convert` sink pad stays busy until the limiter's link is
/// gone). If the relink fails, the previous eq → clipper → post-convert
/// path is re-established **and the owned handle is returned in the
/// retained slot**, so a failed removal leaves the chain with a working
/// (limiter-installed) data path instead of a dangling `eq` source pad, and
/// `clip_protection_installed` keeps matching the routed graph — the caller
/// can no longer record `Off` while the limiter is in the bin (which
/// previously invited a second `clipper` on the next enable).
///
/// If restoration itself fails, the graph is never resumed with an unlinked
/// `eq` source: the partial links are torn down, the limiter leaves the bin,
/// and the direct path is forced. The handle is cleared (no limiter remains
/// installed) and the first tuple member reports whether the requested
/// removal landed.
fn remove_limiter(
    graph: &LimiterGraph,
    clipper: gst::Element,
    direct_blocked: bool,
    restore_blocked: bool,
) -> (bool, Option<gst::Element>) {
    graph.eq.unlink(&clipper);
    clipper.unlink(&graph.post_convert);
    let direct_linked = !direct_blocked && graph.eq.link(&graph.post_convert).is_ok();
    if direct_linked {
        drop_limiter_from_bin(&graph.bin, &clipper);
        return (true, None);
    }
    // Relink failed: restore the working limiter path so the chain stays
    // playable and its tracked ownership keeps matching the routed graph.
    let restored = !restore_blocked
        && graph.eq.link(&clipper).is_ok()
        && clipper.link(&graph.post_convert).is_ok();
    if restored {
        let _ = clipper.sync_state_with_parent();
        return (false, Some(clipper));
    }
    // Restoration itself failed: do not resume an unlinked graph. Tear down
    // any partial restoration link, remove the limiter, and force the direct
    // path so the chain keeps a working route. The handle is cleared because
    // no limiter remains in the bin.
    graph.eq.unlink(&clipper);
    clipper.unlink(&graph.post_convert);
    drop_limiter_from_bin(&graph.bin, &clipper);
    (graph.eq.link(&graph.post_convert).is_ok(), None)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests;
