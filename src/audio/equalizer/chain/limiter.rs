//! Limiter insert/remove surgery for the equalizer bin.
//!
//! Holds the dynamic in-bin topology edit that runs from the blocking pad
//! probe on the `equalizer-10bands` src pad, the engagement handshake that
//! lets the caller cancel only an edit that has demonstrably not started,
//! and the outcome the callback publishes back to the waiting caller.

use std::sync::Mutex;

use gst::prelude::*;
use gstreamer as gst;

use super::super::ClipProtection;
use super::make_element;

/// Return one limiter element to NULL and remove it from the bin.
/// `gst_bin_remove` requires a NULL-state child.
fn drop_limiter_from_bin(bin: &gst::Bin, clipper: &gst::Element) {
    let _ = clipper.set_state(gst::State::Null);
    let _ = bin.remove(clipper);
}

/// The element handles one in-bin limiter edit rewires. Cheap clones of the
/// running graph, so the edit can run from the blocking probe callback on
/// the streaming thread without borrowing the chain.
#[derive(Clone)]
pub(super) struct LimiterGraph {
    pub(super) bin: gst::Bin,
    pub(super) eq: gst::Element,
    pub(super) post_convert: gst::Element,
}

/// The validated state of the `equalizer-10bands` src pad after one limiter
/// topology edit. This is what the blocking-probe callback needs to decide
/// whether it may uninstall itself (contract: the callback never uninstalls
/// the probe while the `equalizer-10bands` src pad has no linked downstream
/// peer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LimiterEditOutcome {
    /// The requested toggle is installed and the graph is linked.
    Installed,
    /// The requested toggle could not be installed, but the pre-edit layout
    /// was restored and the graph is linked; the caller retries on the
    /// pause/relink seam.
    Restored,
    /// The edit and its rollback both failed: the `equalizer-10bands` src
    /// pad has no linked downstream peer. The probe must stay installed and
    /// the wedged pipeline retired by the ordinary teardown seam.
    Unlinked,
}

/// Engagement handshake between [`EqChain::swap_clip_protection_under_block_probe`]
/// and its blocking-probe callback. The caller serializes its cancel decision
/// with the callback's start on this gate, so an edit is either cancelled
/// before it starts or allowed to finish — never cancelled mid-flight on a
/// missing outcome.
#[derive(Default)]
pub(super) struct LimiterEditGate {
    /// Set by the callback before it touches the graph.
    engaged: bool,
    /// Set by the caller when it decides the callback demonstrably has not
    /// started; the callback observes it under the same lock and skips the
    /// edit.
    cancelled: bool,
}

/// Whether the `equalizer-10bands` src pad currently has a linked downstream
/// peer. The pad is the single boundary every limiter edit rewires, so this
/// is the ground truth for the contract's "never uninstall the probe while
/// the EQ src pad has no linked peer" rule.
fn eq_src_has_linked_peer(graph: &LimiterGraph) -> bool {
    graph
        .eq
        .static_pad("src")
        .map(|src| src.peer().is_some())
        .unwrap_or(false)
}

/// Post the contract's explicit error diagnostic for a wedged limiter edit:
/// the message is sourced from the bin, so the ordinary
/// eq-bin-originated bus seam retires the wedged pipeline. The blocking probe
/// stays installed, so no buffer is ever pushed across the unlinked pad and
/// this path cannot produce `GST_FLOW_NOT_LINKED`.
fn post_wedged_topology_error(graph: &LimiterGraph) {
    gst::element_error!(
        graph.bin,
        gst::CoreError::Failed,
        ("equalizer limiter edit left the equalizer-10bands src pad unlinked; \
          retiring the wedged pipeline with the blocking probe still installed")
    );
}

/// The per-edit inputs the blocking-probe callback needs, bundled into one
/// value so the callback stays within the file's parameter budget while the
/// streaming-thread closure captures the pieces once.
pub(super) struct LimiterProbeEdit<'a> {
    pub(super) graph: &'a LimiterGraph,
    pub(super) soft: ClipProtection,
    /// `(direct_blocked, restore_blocked, forced_blocked)`, in the order the
    /// removal surgery consumes them.
    pub(super) decisions: (bool, bool, bool),
    /// Test-only hold that suspends the callback after engagement; never
    /// present in production builds.
    #[cfg(test)]
    pub(super) hold: Option<std::sync::Arc<super::ProbeEditHold>>,
}

/// Run one limiter edit inside the blocking-probe callback and choose the
/// probe's fate. Returns `gst::PadProbeReturn::Remove` only once a linked
/// topology is validated (so blocked flow resumes across a valid chain), or
/// `gst::PadProbeReturn::Ok` — leaving the probe installed — when the edit
/// and its rollback left the `equalizer-10bands` src pad unlinked, with the
/// contract's error diagnostic posted so the ordinary eq-bin bus seam retires
/// the wedged pipeline instead of resuming it. The outcome reaches the
/// waiting caller over `signal`. Extracted from the public seam to keep that
/// method within the file's method-length budget.
///
/// The callback publishes engagement on `gate` before touching the graph,
/// so the caller can only cancel an edit that has demonstrably not started.
pub(super) fn limiter_edit_probe_callback(
    gate: &Mutex<LimiterEditGate>,
    slot: &Mutex<Option<gst::Element>>,
    signal: &Mutex<std::sync::mpsc::SyncSender<LimiterEditOutcome>>,
    edit: LimiterProbeEdit<'_>,
) -> gst::PadProbeReturn {
    // Publish engagement before touching the graph. The caller's cancel
    // decision is serialized on the same gate, so an edit that started is
    // never cancelled and a cancelled edit never starts.
    {
        let mut gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if gate.cancelled {
            return gst::PadProbeReturn::Remove;
        }
        gate.engaged = true;
    }
    let graph = edit.graph;
    let soft = edit.soft;
    let (direct_blocked, restore_blocked, forced_blocked) = edit.decisions;
    #[cfg(test)]
    let hold = edit.hold;
    #[cfg(test)]
    if let Some(hold) = hold.as_ref() {
        hold.enter_and_wait();
    }
    let outcome = {
        let mut current = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        edit_limiter_topology(
            graph,
            soft,
            &mut current,
            direct_blocked,
            restore_blocked,
            forced_blocked,
        )
    };
    if let Ok(tx) = signal.lock() {
        let _ = tx.try_send(outcome);
    }
    if outcome == LimiterEditOutcome::Unlinked {
        // No linked topology exists: keep the probe installed so no buffer is
        // pushed across the unlinked pad, and post the contract's explicit
        // error so the eq-bin bus seam retires the wedged pipeline.
        post_wedged_topology_error(graph);
        gst::PadProbeReturn::Ok
    } else {
        gst::PadProbeReturn::Remove
    }
}

/// What the bounded engagement wait decided about one limiter edit.
///
/// The wait itself is always bounded by
/// [`super::LIMITER_PROBE_ENGAGE_TIMEOUT`]; an engaged-but-unpublished
/// edit is *parked*, never awaited (refinery R1, PR 220 audit): the
/// caller runs synchronously on the GTK thread, so an unbounded wait
/// for a slow or stuck surgery would stall the whole UI and its
/// error/teardown dispatch for the callback's entire duration.
pub(super) enum LimiterEditWait {
    /// The callback published an outcome within the engagement window.
    /// The still-installed probe id travels back so the caller can retire
    /// it over the validated topology.
    Completed(LimiterEditOutcome, Option<gst::PadProbeId>),
    /// The window elapsed while the callback was engaged: the edit is in
    /// flight on the streaming thread and owns the graph mutation and
    /// the `rglimiter` handle. The caller parks the transaction and
    /// adopts the outcome on the main context; it must not remove the
    /// probe and must not wait here. The still-installed probe id
    /// travels back so the parked transaction keeps its ownership.
    Engaged(Option<gst::PadProbeId>),
    /// The window elapsed with no engagement: the edit demonstrably
    /// never started, was marked cancelled under the shared gate, and
    /// its probe was removed inside the wait.
    NotEngagedCancelled,
}

/// Wait — within the bounded engagement window — for the
/// blocking-probe callback to publish an outcome.
///
/// A bounded engagement window may cancel only an edit that has
/// demonstrably not started; once the callback has engaged, the caller
/// parks the transaction ([`LimiterEditWait::Engaged`]) and adopts the
/// published outcome asynchronously, because the edit already owns the
/// graph mutation and the `rglimiter` handle. The probe is removed here
/// only on the not-engaged path.
pub(super) fn await_limiter_edit_outcome(
    eq_src: &gst::Pad,
    mut probe_id: Option<gst::PadProbeId>,
    gate: &Mutex<LimiterEditGate>,
    rx: &std::sync::mpsc::Receiver<LimiterEditOutcome>,
) -> LimiterEditWait {
    match rx.recv_timeout(super::LIMITER_PROBE_ENGAGE_TIMEOUT) {
        Ok(outcome) => LimiterEditWait::Completed(outcome, probe_id),
        Err(_) => {
            let mut gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if gate.engaged {
                // The edit is already executing on the streaming thread.
                // Neither remove its probe nor block this (UI) thread on
                // its outcome: the caller parks the transaction with the
                // probe retained, and the completion is adopted on the
                // main context, so the final topology and the owned
                // handle are retained rather than stranded — while the
                // UI keeps running for the callback's whole duration.
                LimiterEditWait::Engaged(probe_id)
            } else {
                // Demonstrably not started: mark it cancelled under the same
                // gate the callback checks, then retire the probe. A callback
                // that starts afterwards observes `cancelled` and skips the
                // edit.
                gate.cancelled = true;
                drop(gate);
                if let Some(id) = probe_id.take() {
                    eq_src.remove_probe(id);
                }
                LimiterEditWait::NotEngagedCancelled
            }
        }
    }
}

/// Apply one limiter topology edit to `clipper`, the chain's owned
/// `rglimiter` handle (`None` when clip protection is off). The handle is
/// left matching the routed graph, so `clip_protection_installed` stays
/// truthful. Returns the validated topology the edit left behind.
///
/// `direct_blocked`/`restore_blocked`/`forced_blocked` are the test-only
/// fault decisions for the removal surgery; production callers pass `false`.
pub(super) fn edit_limiter_topology(
    graph: &LimiterGraph,
    soft: ClipProtection,
    clipper: &mut Option<gst::Element>,
    direct_blocked: bool,
    restore_blocked: bool,
    forced_blocked: bool,
) -> LimiterEditOutcome {
    let requested_installed = match (soft, clipper.take()) {
        (ClipProtection::Soft, None) => match insert_limiter(graph) {
            Ok(installed) => {
                *clipper = Some(installed);
                true
            }
            Err(()) => false,
        },
        (ClipProtection::Off, Some(installed)) => {
            let (landed, retained) = remove_limiter(
                graph,
                installed,
                direct_blocked,
                restore_blocked,
                forced_blocked,
            );
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
    };
    // The peer check overrides the surgery's own success flag: an edit (or a
    // no-op) that leaves the EQ src pad unlinked is never a resumable
    // topology, even if the requested toggle nominally landed.
    if !eq_src_has_linked_peer(graph) {
        LimiterEditOutcome::Unlinked
    } else if requested_installed {
        LimiterEditOutcome::Installed
    } else {
        LimiterEditOutcome::Restored
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
/// removal landed. If that final forced direct link also fails, the first
/// member is `false`, the handle is `None`, and the caller observes the
/// unlinked `eq` src pad and leaves the probe installed.
fn remove_limiter(
    graph: &LimiterGraph,
    clipper: gst::Element,
    direct_blocked: bool,
    restore_blocked: bool,
    forced_blocked: bool,
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
    // no limiter remains in the bin; a failed forced link leaves the `eq`
    // src pad unlinked for the caller's probe-installed wedge path.
    graph.eq.unlink(&clipper);
    clipper.unlink(&graph.post_convert);
    drop_limiter_from_bin(&graph.bin, &clipper);
    let forced_linked = !forced_blocked && graph.eq.link(&graph.post_convert).is_ok();
    (forced_linked, None)
}
