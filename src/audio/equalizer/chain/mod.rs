//! The local-pipeline equalizer filter bin and its live-reconfiguration
//! transactions (contract: *Filter graph* and *Band and preamp
//! mechanics*).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use super::{ClipProtection, EqSettings};

mod limiter;

use limiter::{
    await_limiter_edit_outcome, edit_limiter_topology, install_limiter_edit_probe,
    LimiterEditOutcome, LimiterEditWait, LimiterGraph,
};

/// How long the dynamic limiter edit waits, after installing its blocking
/// pad probe, for the callback to publish an outcome or to demonstrably
/// engage. This is the *engagement* window: a pad that never reports idle
/// can never stall the UI thread, and an edit that has not started when
/// the window closes is cancelled. Once the callback has engaged, the
/// caller parks the transaction ([`PendingLimiterEdit`]) instead of
/// waiting: the completion is adopted on the main context, so a slow or
/// stuck surgery can never block the (GTK) caller thread.
const LIMITER_PROBE_ENGAGE_TIMEOUT: Duration = Duration::from_millis(200);

/// An engaged limiter edit parked by its caller instead of being awaited
/// (refinery R1, PR 220 audit). The blocking-probe callback engaged — it
/// owns the graph mutation and the `rglimiter` handle in its shared slot —
/// but had not published its outcome within the bounded engagement window,
/// so the caller stored the whole transaction here and returned without
/// blocking its (UI) thread.
///
/// While a transaction is parked, the edit is *in flight on an unvalidated
/// graph*: no second topology edit and no pause/relink fallback may run
/// ([`EqChain::has_pending_limiter_edit`] gates both), and
/// [`EqChain::clip_protection_installed`] reports the pre-edit truth the
/// transaction recorded, because the callback holds the real handle in its
/// slot. The receiver side of the outcome channel is adopted on the main
/// context by [`EqChain::poll_pending_limiter_edit`], which never blocks.
struct PendingLimiterEdit {
    /// The callback publishes exactly one outcome here; polled
    /// non-blockingly from the main context.
    rx: std::sync::mpsc::Receiver<LimiterEditOutcome>,
    /// The `rglimiter` handle slot shared with the callback: the surgery
    /// moves the owned handle through it, and adoption takes it back.
    slot: Arc<Mutex<Option<gst::Element>>>,
    /// The blocking probe, still installed while the edit is in flight.
    /// Removed at adoption only when the outcome validated a linked
    /// topology; a wedged outcome keeps it installed over the unlinked
    /// pad.
    probe_id: Option<gst::PadProbeId>,
    /// The pad the probe is installed on (retained so the id stays
    /// removable at adoption).
    eq_src: gst::Pad,
    /// The protection the edit was asked to install.
    requested: ClipProtection,
    /// Whether the limiter was routed when the edit started. The callback
    /// owns the handle in `slot` while in flight, so the caller-side
    /// field cannot answer truthfully until adoption.
    pre_edit_installed: bool,
    /// Test-only retained sender so a regression can decide when (or
    /// whether) the parked outcome publishes. Never present in
    /// production.
    #[cfg(test)]
    test_tx: Option<std::sync::mpsc::SyncSender<LimiterEditOutcome>>,
}

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

/// Test-only deterministic fault injection for the limiter-removal
/// surgery. The variants model the failure shapes the production code
/// path must survive: the direct relink is refused once; the direct
/// relink and the limiter-path restoration are both refused; or every
/// link the surgery can make — including the final forced direct
/// link — is refused, leaving the `equalizer-10bands` src pad with no
/// peer. Each fault is consumed when it fires, so a subsequent call
/// retries against the real graph — exactly the retry the caller's
/// recorded-state discipline depends on.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimiterRemoveFault {
    /// The direct `eq → post-convert` relink is refused; the
    /// limiter-path restoration then succeeds.
    DirectRelinkBlocked,
    /// Both the direct relink and the limiter-path restoration fail;
    /// the final forced direct link then succeeds.
    DirectRelinkAndRestore,
    /// The direct relink, the limiter-path restoration, and the final
    /// forced direct link all fail, leaving the `equalizer-10bands` src
    /// pad unlinked.
    EveryLinkBlocked,
}

/// Test-only deterministic seam that suspends the blocking-probe callback
/// *after* it has engaged the edit, until a regression test releases it.
///
/// The engagement handshake only exists to cancel an edit that has
/// demonstrably not started; an asynchronous callback that is already
/// executing must never have its probe removed on a missing outcome. This
/// hold lets the tests reproduce exactly that interleaving on a live
/// pipeline: the callback engages, blocks here while the caller's bounded
/// engagement window expires, and then completes so the caller can be
/// observed to retain the outcome, the probe, and the limiter handle.
#[cfg(test)]
struct ProbeEditHold {
    state: Mutex<ProbeEditHoldState>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
struct ProbeEditHoldState {
    entered: bool,
    released: bool,
    /// The thread the held callback is running on (the streaming thread).
    thread: Option<std::thread::ThreadId>,
}

#[cfg(test)]
impl ProbeEditHold {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProbeEditHoldState {
                entered: false,
                released: false,
                thread: None,
            }),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Called on the streaming thread from inside the probe callback:
    /// publish entry, then block until the test releases the callback.
    fn enter_and_wait(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.entered = true;
        state.thread = Some(std::thread::current().id());
        self.cv.notify_all();
        while !state.released {
            state = self
                .cv
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Block (bounded) until the callback has entered the hold.
    fn wait_until_entered(&self, timeout: Duration) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (state, _) = self
            .cv
            .wait_timeout_while(state, timeout, |state| !state.entered)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.entered
    }

    /// The thread the held callback is running on, once it has entered.
    fn callback_thread(&self) -> Option<std::thread::ThreadId> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .thread
    }

    /// Whether the hold has been released (the surgery is free to
    /// finish).
    fn is_released(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .released
    }

    /// Let the held callback run to completion.
    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.released = true;
        self.cv.notify_all();
    }
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
    /// Set when a limiter edit and its rollback could not validate a
    /// linked topology: the `equalizer-10bands` src pad has no peer and
    /// the blocking probe was left installed. Once wedged, no further
    /// topology edit is attempted; the pipeline is retired by the
    /// eq-bin-originated bus error seam instead of resumed.
    wedged: bool,
    /// An engaged limiter edit parked instead of awaited: the callback
    /// owns the graph until its outcome is adopted on the main context.
    pending_edit: Option<PendingLimiterEdit>,
    /// Test-only armed surgery fault; production builds never carry it.
    #[cfg(test)]
    remove_fault: Option<LimiterRemoveFault>,
    /// Test-only seam that suspends the blocking-probe callback after
    /// engagement; production builds never carry it.
    #[cfg(test)]
    probe_hold: Option<Arc<ProbeEditHold>>,
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
            wedged: false,
            pending_edit: None,
            #[cfg(test)]
            remove_fault: None,
            #[cfg(test)]
            probe_hold: None,
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
    ///
    /// A wedged chain (an earlier edit and rollback left the
    /// `equalizer-10bands` src pad unlinked) refuses every further edit: no
    /// topology change can validate a linked graph, and the contract retires
    /// the wedged pipeline instead of resuming it, so reporting a successful
    /// toggle here would be untruthful.
    pub fn set_clip_protection(&mut self, soft: ClipProtection) -> bool {
        if self.wedged {
            return false;
        }
        let graph = self.limiter_graph();
        // Precompute the test-fault decisions in the order the surgery
        // consumes them: the direct relink is always attempted, the
        // restoration only after the direct relink is refused, and the
        // forced direct link only after the restoration is refused.
        let (direct_blocked, restore_blocked, forced_blocked) = self.take_limiter_fault_decisions();
        let outcome = edit_limiter_topology(
            &graph,
            soft,
            &mut self.clipper,
            direct_blocked,
            restore_blocked,
            forced_blocked,
        );
        if outcome == LimiterEditOutcome::Unlinked {
            self.wedged = true;
        }
        outcome == LimiterEditOutcome::Installed
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
    /// When even the rollback cannot re-link the `equalizer-10bands` src
    /// pad, the callback keeps the probe installed, posts the explicit
    /// error diagnostic, and lets the ordinary eq-bin-originated bus seam
    /// retire the wedged pipeline — it never resumes flow across an
    /// unlinked pad (contract: *Live-reconfiguration boundary*).
    ///
    /// A pipeline that never reports the pad idle within the bounded
    /// engagement window is left untouched and the caller falls back to the
    /// pause/relink seam. The window only ever cancels an edit that has
    /// **demonstrably not started**. Once the callback engages, the edit
    /// already owns the graph mutation and the `rglimiter` handle —
    /// cancelling it would strand both, and waiting for it on this
    /// (synchronous, UI-reachable) call path would stall the UI for the
    /// callback's whole duration. The engaged edit is therefore parked as
    /// a pending transaction and its outcome is adopted on the main
    /// context (refinery R1); until adoption, no second edit and no
    /// pause/relink fallback may run over the in-flight, unvalidated
    /// graph.
    ///
    /// Returns `Some(true)` when the requested toggle is installed,
    /// `Some(false)` when the dynamic re-link failed and the pre-edit
    /// layout was restored or the graph was left wedged (the caller retries
    /// via the pause/relink seam, which refuses a wedged chain), and `None`
    /// when the probe could not engage, no EQ src pad exists, **or the edit
    /// engaged and outlived the engagement window** — the caller then
    /// distinguishes the parked case with
    /// [`EqChain::has_pending_limiter_edit`] and must run no fallback:
    /// the outcome is adopted on the main context (refinery R1).
    pub fn swap_clip_protection_under_block_probe(&mut self, soft: ClipProtection) -> Option<bool> {
        if self.wedged {
            return Some(false);
        }
        if self.pending_edit.is_some() {
            // An engaged edit is still in flight: it owns the graph
            // mutation and the limiter handle, so a second edit must not
            // install another probe over the unvalidated graph
            // (refinery R1: no conflicting edit over an in-flight edit).
            return None;
        }
        let eq_src = self.eq.static_pad("src")?;
        // The truth the caller must record if the edit outlives the
        // engagement window: while the transaction is parked the callback
        // owns the handle, so `clip_protection_installed` answers from
        // this recorded value.
        let pre_edit_installed = self.clipper.is_some();
        let edit = install_limiter_edit_probe(self, &eq_src, soft);
        // A pad that reports idle synchronously runs the callback before
        // `add_probe` returns and reports no id; either way the outcome
        // arrives over the channel. A missing outcome within the bounded
        // window means the callback either never engaged (cancel it) or
        // already started (park the transaction and adopt its outcome on
        // the main context — never block this caller for it).
        match await_limiter_edit_outcome(&eq_src, edit.probe_id, &edit.gate, &edit.rx) {
            LimiterEditWait::Completed(outcome, probe_id) => {
                self.adopt_limiter_edit_outcome(&eq_src, &edit.slot, Some(outcome), false, probe_id)
            }
            LimiterEditWait::NotEngagedCancelled => None,
            LimiterEditWait::Engaged(probe_id) => {
                // Park the whole transaction: the probe stays installed,
                // the handle stays owned by the callback's slot, and the
                // outcome receiver moves into the chain for main-context
                // adoption. The caller observes `None` +
                // `has_pending_limiter_edit()` and runs no fallback.
                self.pending_edit = Some(PendingLimiterEdit {
                    rx: edit.rx,
                    slot: edit.slot,
                    probe_id,
                    eq_src: eq_src.clone(),
                    requested: soft,
                    pre_edit_installed,
                    #[cfg(test)]
                    test_tx: None,
                });
                None
            }
        }
    }

    /// Retire the blocking probe over a validated topology, adopt the
    /// `rglimiter` handle the callback left behind, and record the wedge when
    /// the edit could not validate a linked graph.
    ///
    /// An engaged edit whose outcome never arrived is conservatively treated
    /// as wedged: the probe stays installed and the fallback is refused, so
    /// it can never resume a graph whose topology was not validated.
    fn adopt_limiter_edit_outcome(
        &mut self,
        eq_src: &gst::Pad,
        slot: &Mutex<Option<gst::Element>>,
        outcome: Option<LimiterEditOutcome>,
        engagement_confirmed: bool,
        probe_id: Option<gst::PadProbeId>,
    ) -> Option<bool> {
        let wedged = outcome == Some(LimiterEditOutcome::Unlinked)
            || (engagement_confirmed && outcome.is_none());
        if let Some(probe_id) = probe_id {
            // Harmless when the callback already uninstalled itself over a
            // validated topology. A wedged callback returned `Ok`, so the
            // probe must stay installed and is deliberately not removed.
            if !wedged {
                eq_src.remove_probe(probe_id);
            }
        }
        // Adopt the handle the callback left in the slot (or the original
        // handle when the callback never ran).
        self.clipper = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if wedged {
            self.wedged = true;
        }
        outcome.map(|outcome| outcome == LimiterEditOutcome::Installed)
    }

    /// Test-only seam: arm a hold that suspends the blocking-probe callback
    /// after it has engaged, until the test releases it.
    #[cfg(test)]
    fn inject_probe_edit_hold(&mut self, hold: Arc<ProbeEditHold>) {
        self.probe_hold = Some(hold);
    }

    /// Test-only: resolve and consume the armed surgery fault into the
    /// three link decisions the removal surgery takes, in the order it
    /// consumes them — direct relink, limiter-path restoration, and the
    /// final forced direct link. Consumed once, so a later call retries the
    /// real graph. Always `(false, false, false)` in production builds.
    // The production body never reads the (test-only) fault field, so the
    // receiver is genuinely unused there; allow the pedantic lints rather
    // than split the method across cfg signatures.
    #[cfg_attr(
        not(test),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    fn take_limiter_fault_decisions(&mut self) -> (bool, bool, bool) {
        #[cfg(test)]
        {
            match self.remove_fault.take() {
                Some(LimiterRemoveFault::DirectRelinkBlocked) => (true, false, false),
                Some(LimiterRemoveFault::DirectRelinkAndRestore) => (true, true, false),
                Some(LimiterRemoveFault::EveryLinkBlocked) => (true, true, true),
                None => (false, false, false),
            }
        }
        #[cfg(not(test))]
        {
            (false, false, false)
        }
    }

    /// Test-only seam: arm a deterministic limiter-removal fault at the
    /// real surgery boundary.
    #[cfg(test)]
    pub(crate) fn inject_limiter_remove_fault(&mut self, fault: LimiterRemoveFault) {
        self.remove_fault = Some(fault);
    }

    /// True when the `rglimiter` element is currently inside the bin.
    ///
    /// Pending-edit aware: while a parked transaction is in flight the
    /// callback owns the handle in its shared slot, so the caller-side
    /// field would wrongly report "off"; the transaction's recorded
    /// pre-edit truth answers instead (refinery R1).
    #[allow(dead_code)] // inspection helper; exercised by the contract tests
    pub fn clip_protection_installed(&self) -> bool {
        match &self.pending_edit {
            Some(pending) => pending.pre_edit_installed,
            None => self.clipper.is_some(),
        }
    }

    /// True while a limiter edit is in flight under a parked transaction:
    /// the callback engaged on the streaming thread, and its outcome has
    /// not been adopted yet. While this is `true` the graph is
    /// unvalidated mid-surgery, so no second topology edit and no
    /// pause/relink fallback may run (refinery R1).
    #[allow(dead_code)] // inspection helper; exercised by the contract tests
    pub fn has_pending_limiter_edit(&self) -> bool {
        self.pending_edit.is_some()
    }

    /// The protection the in-flight edit was asked to install, or `None`
    /// when no transaction is parked.
    pub fn pending_limiter_edit_request(&self) -> Option<ClipProtection> {
        self.pending_edit.as_ref().map(|pending| pending.requested)
    }

    /// Adopt a parked edit's completion without blocking: `None` while the
    /// callback has not published yet, `Some(installed)` once an outcome
    /// arrived and the transaction settled. This is the *main-context*
    /// completion path (refinery R1) — the synchronous caller never waits
    /// for it.
    ///
    /// Adoption mirrors the synchronous path's semantics: the handle the
    /// callback left in the slot is taken back, the probe is retired when
    /// the outcome validated a linked topology and kept when the edit
    /// wedged, and the wedge flag records an unvalidated final topology.
    /// A channel disconnected *without* an outcome — the callback can
    /// never publish — is treated exactly like the synchronous path's
    /// engagement-confirmed missing outcome: conservatively wedged.
    pub fn poll_pending_limiter_edit(&mut self) -> Option<bool> {
        let outcome = {
            let pending = self.pending_edit.as_mut()?;
            match pending.rx.try_recv() {
                Ok(outcome) => outcome,
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                // The callback can never publish (its signal sender is
                // gone without a send): conservatively wedged.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => LimiterEditOutcome::Unlinked,
            }
        };
        let pending = self.pending_edit.take()?;
        let wedged = outcome == LimiterEditOutcome::Unlinked;
        if !wedged {
            // Harmless when the callback already uninstalled itself over a
            // validated topology. A wedged callback returned `Ok`, so the
            // probe must stay installed and is deliberately not removed.
            if let Some(probe_id) = pending.probe_id {
                pending.eq_src.remove_probe(probe_id);
            }
        }
        // Adopt the handle the callback left in the slot.
        self.clipper = pending
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if wedged {
            self.wedged = true;
        }
        Some(outcome == LimiterEditOutcome::Installed)
    }

    /// Park a test-controlled transaction as if an edit had engaged and
    /// outlived the engagement window. Production builds never carry it.
    /// The transaction keeps the sender, so
    /// [`EqChain::complete_pending_limiter_edit`] decides when the
    /// outcome publishes.
    #[cfg(test)]
    pub(crate) fn inject_pending_limiter_edit(&mut self, requested: ClipProtection) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let pre_edit_installed = self.clipper.is_some();
        self.pending_edit = Some(PendingLimiterEdit {
            rx,
            slot: Arc::new(Mutex::new(self.clipper.take())),
            probe_id: None,
            eq_src: self.eq.static_pad("src").expect("eq src pad"),
            requested,
            pre_edit_installed,
            test_tx: Some(tx),
        });
    }

    /// Complete a test-parked transaction as if the callback had
    /// published: `true` publishes `Installed`, `false` publishes
    /// `Restored` (a valid rollback). Returns whether the publication
    /// succeeded.
    #[cfg(test)]
    pub(crate) fn complete_pending_limiter_edit(&mut self, installed: bool) -> bool {
        let outcome = if installed {
            LimiterEditOutcome::Installed
        } else {
            LimiterEditOutcome::Restored
        };
        let tx = self
            .pending_edit
            .as_mut()
            .and_then(|pending| pending.test_tx.take());
        if installed {
            // Keep the shortcut faithful to the real callback: after a
            // validated removal the callback has dropped the original
            // limiter handle, so adoption must find the slot empty and
            // record no limiter. (`Restored` publications leave the
            // original handle in the slot, as the real rollback does.)
            if let Some(pending) = self.pending_edit.as_ref() {
                if let Ok(mut slot) = pending.slot.lock() {
                    slot.take();
                }
            }
        }
        tx.map(|tx| tx.send(outcome).is_ok()).unwrap_or(false)
    }

    /// True when a limiter edit and its rollback could not validate a linked
    /// topology. A wedged chain refuses further edits; the blocking probe
    /// stays installed and the ordinary eq-bin-originated bus seam retires
    /// the wedged pipeline.
    #[allow(dead_code)] // inspection helper; exercised by the contract tests
    pub fn topology_wedged(&self) -> bool {
        self.wedged
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // contract-fixed gains (±0.0/0.5-steps) are exact in f64
mod tests;

#[cfg(test)]
mod live_probe_tests;
