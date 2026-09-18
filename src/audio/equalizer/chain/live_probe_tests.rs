//! Live-pipeline regressions for the asynchronous blocking-probe limiter
//! edit: the completion/timeout race where an already-executing callback
//! must not have its probe removed on a missing channel result.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::tests::{
    assert_links_eq_directly_to_post_convert, assert_links_eq_through_clipper, bin_requires_plugins,
};
use super::*;

/// Live `audiotestsrc → eq-bin → fakesink` fixture used by the
/// asynchronous blocking-probe regressions. A `BLOCK_DOWNSTREAM | IDLE`
/// probe on the `equalizer-10bands` src pad is dispatched by the
/// streaming thread here, rather than synchronously by `add_probe` on a
/// non-streaming bin, which is what lets a test interleave the caller's
/// bounded engagement window with an already-executing edit.
struct LiveEqChain {
    chain: EqChain,
    pipeline: gst::Pipeline,
    sink_pad: gst::Pad,
    delivery_probe: Option<gst::PadProbeId>,
}

impl LiveEqChain {
    fn start(clip_protection: ClipProtection) -> Option<Self> {
        if !bin_requires_plugins() {
            return None;
        }
        let chain = EqChain::build(&EqSettings {
            enabled: true,
            clip_protection,
            ..EqSettings::default()
        })
        .expect("eq-bin builds");
        let (pipeline, sink_pad, delivery_probe, delivered) = start_live_pipeline(&chain);
        wait_for_live_buffers(&delivered);
        Some(Self {
            chain,
            pipeline,
            sink_pad,
            delivery_probe: Some(delivery_probe),
        })
    }

    fn stop(&mut self) {
        if let Some(delivery_probe) = self.delivery_probe.take() {
            self.sink_pad.remove_probe(delivery_probe);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Assemble and start the live `audiotestsrc → eq-bin → fakesink` pipeline,
/// returning the handles the fixture owns plus the delivered-buffer counter.
fn start_live_pipeline(
    chain: &EqChain,
) -> (gst::Pipeline, gst::Pad, gst::PadProbeId, Arc<AtomicUsize>) {
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .expect("live audio source");
    // `sync=true` makes the sink pace to the clock, so the streaming
    // thread is inside a push through the eq src pad for most of each
    // buffer cycle. That keeps the pad busy when the probe is installed
    // and forces the `IDLE` probe to be dispatched on the streaming
    // thread (deferred), instead of synchronously by `add_probe`.
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", true)
        .build()
        .expect("audio sink");
    let bin = chain.bin.clone().upcast::<gst::Element>();
    pipeline
        .add_many([&source, &bin, &sink])
        .expect("assemble live eq pipeline");
    gst::Element::link_many([&source, &bin, &sink]).expect("link live eq pipeline");

    let delivered = Arc::new(AtomicUsize::new(0));
    let delivered_from_probe = Arc::clone(&delivered);
    let sink_pad = sink.static_pad("sink").expect("sink pad");
    let delivery_probe = sink_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            delivered_from_probe.fetch_add(1, Ordering::AcqRel);
            gst::PadProbeReturn::Ok
        })
        .expect("delivery counter probe");

    pipeline
        .set_state(gst::State::Playing)
        .expect("start live eq pipeline");
    (pipeline, sink_pad, delivery_probe, delivered)
}

/// Block until the live pipeline has begun delivering buffers.
fn wait_for_live_buffers(delivered: &AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while delivered.load(Ordering::Acquire) < 5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        delivered.load(Ordering::Acquire) >= 5,
        "live eq pipeline did not begin delivering buffers"
    );
}

/// Arm a held callback, run one clip-protection swap on the caller's
/// thread, and return the *adopted* outcome plus the thread the held
/// callback ran on. The swap must **return while the callback is still
/// held** — a parked transaction, never an awaited one (refinery R1) —
/// so the helper asserts the parked invariants and the main context's
/// progress at that moment, then releases, adopts the completion with a
/// bounded poll, and reports it.
fn swap_across_engagement_window(
    live: &mut LiveEqChain,
    hold: &Arc<ProbeEditHold>,
    target: ClipProtection,
) -> (Option<bool>, Option<std::thread::ThreadId>) {
    live.chain.inject_probe_edit_hold(Arc::clone(hold));
    let releaser = std::thread::spawn({
        let hold = Arc::clone(hold);
        move || {
            assert!(
                hold.wait_until_entered(Duration::from_secs(5)),
                "the async probe callback never engaged"
            );
            // Hold the executing edit across the caller's engagement
            // window: an unconditional cancel would remove the probe here.
            std::thread::sleep(LIMITER_PROBE_ENGAGE_TIMEOUT + Duration::from_millis(100));
            hold.release();
        }
    });
    let result = live.chain.swap_clip_protection_under_block_probe(target);
    assert!(
        hold.wait_until_entered(Duration::from_secs(5)),
        "the async probe callback never engaged"
    );
    assert!(
        !hold.is_released(),
        "the caller must return from the swap while the surgery is still held"
    );
    assert_eq!(
        result, None,
        "a parked edit reports no synchronous outcome: the caller did not wait for the surgery"
    );
    assert!(
        live.chain.has_pending_limiter_edit(),
        "the engaged edit must be parked as a pending transaction"
    );
    assert_main_context_progresses_while_held();
    releaser.join().expect("releaser thread");
    let adopted = adopt_pending_with_deadline(live);
    (adopted, hold.callback_thread())
}

/// While the surgery is held on the streaming thread, the caller's main
/// context — the thread the UI runs on — must keep making progress:
/// attach an idle source, run iterations, and require the source to
/// execute. This is the responsiveness the pre-R1 unbounded wait denied.
/// The context is PRIVATE: regression tests run in parallel threads and
/// the default main context can be owned by at most one of them.
fn assert_main_context_progresses_while_held() {
    let context = gst::glib::MainContext::new();
    let progressed = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&progressed);
    let source = gst::glib::idle_source_new(None, gst::glib::Priority::DEFAULT, move || {
        flag.fetch_add(1, Ordering::AcqRel);
        gst::glib::ControlFlow::Break
    });
    source.attach(Some(&context));
    let deadline = Instant::now() + Duration::from_secs(5);
    let ran = context
        .with_thread_default(|| {
            while progressed.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                context.iteration(false);
            }
            progressed.load(Ordering::Acquire) > 0
        })
        .unwrap_or(false);
    assert!(
        ran,
        "the main context made no progress while the edit was held"
    );
}

/// Poll the parked transaction until the released callback publishes,
/// bounded, and return the adoption outcome.
fn adopt_pending_with_deadline(live: &mut LiveEqChain) -> Option<bool> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(installed) = live.chain.poll_pending_limiter_edit() {
            return Some(installed);
        }
        assert!(
            Instant::now() < deadline,
            "the parked edit never published its outcome"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Regression (audit 2026-09-17, PR #220): a completion/timeout race must
/// not remove an already-executing edit's blocking probe. The callback is
/// held *after* engagement across the caller's bounded engagement window
/// and then completes with a valid rollback (direct relink refused once,
/// limiter path restored). The caller must observe the rollback — wedge
/// false, probe retired, limiter ownership retained — rather than
/// cancelling the edit and reporting an unconfirmed `None`.
#[test]
fn async_probe_edit_crossing_the_engagement_window_retains_a_valid_rollback() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Soft) else {
        return;
    };
    assert!(live.chain.clip_protection_installed());
    let eq_src = live.chain.eq.static_pad("src").expect("eq src pad");
    assert!(
        !eq_src.is_blocked(),
        "no probe is installed before the edit"
    );
    live.chain
        .inject_limiter_remove_fault(LimiterRemoveFault::DirectRelinkBlocked);

    let hold = Arc::new(ProbeEditHold::new());
    let caller_thread = std::thread::current().id();
    let (result, callback_thread) =
        swap_across_engagement_window(&mut live, &hold, ClipProtection::Off);

    assert_ne!(
        callback_thread,
        Some(caller_thread),
        "the probe callback must run on the streaming thread for this regression"
    );
    assert_eq!(
        result,
        Some(false),
        "the caller must observe the valid rollback via the asynchronous adoption, \
         not cancel the executing edit"
    );
    assert!(
        !live.chain.topology_wedged(),
        "a topology restored by the rollback is not wedged"
    );
    assert!(
        live.chain.clip_protection_installed(),
        "the rollback keeps the limiter routed, so ownership truth stays installed"
    );
    assert!(
        !eq_src.is_blocked(),
        "a validated rollback must retire the blocking probe"
    );
    assert!(eq_src.peer().is_some(), "the restored graph stays linked");
    assert_links_eq_through_clipper(&live.chain.bin);

    // The caller's pause/relink fallback now runs the real retry.
    live.stop();
    assert!(live.chain.set_clip_protection(ClipProtection::Off));
    assert!(!live.chain.clip_protection_installed());
    assert_links_eq_directly_to_post_convert(&live.chain.bin);
}

/// Regression (audit 2026-09-17, PR #220): the same completion/timeout
/// race ending in an *unlinked* rollback must retain the blocking probe,
/// record the wedge, clear the limiter handle, and refuse the fallback —
/// the fallback can never resume an unlinked graph.
#[test]
fn async_probe_edit_crossing_the_engagement_window_keeps_an_unlinked_rollback_wedged() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Soft) else {
        return;
    };
    assert!(live.chain.clip_protection_installed());
    let eq_src = live.chain.eq.static_pad("src").expect("eq src pad");
    live.chain
        .inject_limiter_remove_fault(LimiterRemoveFault::EveryLinkBlocked);

    let hold = Arc::new(ProbeEditHold::new());
    let caller_thread = std::thread::current().id();
    let (result, callback_thread) =
        swap_across_engagement_window(&mut live, &hold, ClipProtection::Off);

    assert_ne!(
        callback_thread,
        Some(caller_thread),
        "the probe callback must run on the streaming thread for this regression"
    );
    assert_eq!(
        result,
        Some(false),
        "an unlinked rollback must report the requested toggle as unmet"
    );
    assert!(
        eq_src.is_blocked(),
        "the probe must stay installed across the unlinked pad"
    );
    assert!(
        eq_src.peer().is_none(),
        "the failed edit and rollback leave the eq src pad unlinked"
    );
    assert!(!live.chain.clip_protection_installed());
    assert!(live.chain.topology_wedged());

    // The fallback cannot fabricate a successful toggle over the unlinked
    // graph, and it must not disturb the retained probe.
    assert!(!live.chain.set_clip_protection(ClipProtection::Off));
    assert!(live.chain.topology_wedged());
    assert!(eq_src.is_blocked());
    assert!(eq_src.peer().is_none());

    live.stop();
}

/// Regression (refinery R1, PR 220 audit): a *clean insertion* that
/// engages and outlives the engagement window parks its transaction, the
/// caller returns while the surgery runs, and the main-context adoption
/// records the installed limiter — the caller-side `None` is a park, not
/// a failure.
#[test]
fn pending_edit_across_the_engagement_window_adopts_a_clean_insertion() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Off) else {
        return;
    };
    assert!(!live.chain.clip_protection_installed());
    let eq_src = live.chain.eq.static_pad("src").expect("eq src pad");
    assert!(!eq_src.is_blocked());

    let hold = Arc::new(ProbeEditHold::new());
    let (result, _callback_thread) =
        swap_across_engagement_window(&mut live, &hold, ClipProtection::Soft);

    assert_eq!(
        result,
        Some(true),
        "the adopted outcome reports the requested toggle as installed"
    );
    assert!(
        !live.chain.has_pending_limiter_edit(),
        "the transaction is settled after adoption"
    );
    assert!(live.chain.clip_protection_installed());
    assert!(!live.chain.topology_wedged());
    assert!(
        !eq_src.is_blocked(),
        "a validated insertion retires the blocking probe"
    );
    assert_links_eq_through_clipper(&live.chain.bin);

    live.stop();
}

/// Regression (refinery R1, PR 220 audit): the *clean removal* direction
/// through the parked path adopts `Installed`, retires the probe, and
/// leaves the direct post-convert link routed.
#[test]
fn pending_edit_across_the_engagement_window_adopts_a_clean_removal() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Soft) else {
        return;
    };
    assert!(live.chain.clip_protection_installed());
    let eq_src = live.chain.eq.static_pad("src").expect("eq src pad");

    let hold = Arc::new(ProbeEditHold::new());
    let (result, _callback_thread) =
        swap_across_engagement_window(&mut live, &hold, ClipProtection::Off);

    assert_eq!(
        result,
        Some(true),
        "the adopted outcome reports the requested removal as installed"
    );
    assert!(!live.chain.has_pending_limiter_edit());
    assert!(!live.chain.clip_protection_installed());
    assert!(!live.chain.topology_wedged());
    assert!(!eq_src.is_blocked());
    assert_links_eq_directly_to_post_convert(&live.chain.bin);

    live.stop();
}

/// Regression (refinery R1, PR 220 audit): while an edit is parked, a
/// second swap is refused without installing another probe — no
/// conflicting edit may run over an in-flight, unvalidated graph — and
/// the adopted topology is the *first* edit's request.
#[test]
fn a_second_edit_is_refused_while_a_transaction_is_parked() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Soft) else {
        return;
    };
    let hold = Arc::new(ProbeEditHold::new());
    live.chain.inject_probe_edit_hold(Arc::clone(&hold));
    let releaser = std::thread::spawn({
        let hold = Arc::clone(&hold);
        move || {
            assert!(hold.wait_until_entered(Duration::from_secs(5)));
            std::thread::sleep(LIMITER_PROBE_ENGAGE_TIMEOUT + Duration::from_millis(100));
            hold.release();
        }
    });

    // First edit: removal (Off) — parks once the callback engages.
    let first = live
        .chain
        .swap_clip_protection_under_block_probe(ClipProtection::Off);
    assert_eq!(first, None, "the engaged edit parks instead of waiting");
    assert!(live.chain.has_pending_limiter_edit());

    // Second edit over the in-flight graph: refused outright.
    let second = live
        .chain
        .swap_clip_protection_under_block_probe(ClipProtection::Soft);
    assert_eq!(
        second, None,
        "a conflicting edit over a parked transaction reports no outcome"
    );
    assert!(
        live.chain.has_pending_limiter_edit(),
        "the second edit must not replace or disturb the parked transaction"
    );
    assert_main_context_progresses_while_held();

    releaser.join().expect("releaser thread");
    let adopted = adopt_pending_with_deadline(&mut live);
    assert_eq!(adopted, Some(true), "the first edit's removal is adopted");
    assert!(
        !live.chain.clip_protection_installed(),
        "the settled topology is the FIRST edit's request: the limiter is removed"
    );
    assert!(!live.chain.topology_wedged());

    live.stop();
}

/// Regression (refinery R1, PR 220 audit): an engaged edit whose outcome
/// can never publish — the callback's sender is gone without a send — is
/// adopted conservatively as a wedge: the probe logic cannot resume a
/// topology that was never validated, and the chain refuses further
/// edits while the bus seam retires the pipeline.
#[test]
fn a_pending_edit_that_never_publishes_wedges_conservatively() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Soft) else {
        return;
    };
    let eq_src = live.chain.eq.static_pad("src").expect("eq src pad");
    assert!(live.chain.clip_protection_installed());

    // Park a transaction whose outcome channel is already disconnected:
    // the callback can never publish.
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(tx);
        let pre_edit_installed = live.chain.clipper.is_some();
        live.chain.pending_edit = Some(super::PendingLimiterEdit {
            rx,
            slot: std::sync::Arc::new(std::sync::Mutex::new(live.chain.clipper.take())),
            probe_id: None,
            eq_src: eq_src.clone(),
            requested: ClipProtection::Off,
            pre_edit_installed,
            test_tx: None,
        });
    }
    assert!(
        live.chain.clip_protection_installed(),
        "the parked transaction reports the pre-edit truth while in flight"
    );
    assert_eq!(
        live.chain.pending_limiter_edit_request(),
        Some(ClipProtection::Off)
    );

    assert_eq!(
        live.chain.poll_pending_limiter_edit(),
        Some(false),
        "a never-publishing edit is adopted as an unmet toggle"
    );
    assert!(
        live.chain.topology_wedged(),
        "the never-validated topology is recorded as wedged"
    );
    assert!(!live.chain.has_pending_limiter_edit());
    assert!(
        live.chain.clip_protection_installed(),
        "adoption takes the handle the slot preserved back into the chain"
    );
    assert!(
        !live.chain.set_clip_protection(ClipProtection::Off),
        "a wedged chain refuses the pause/relink fallback"
    );

    live.stop();
}

/// Regression (refinery R1, PR 220 audit): a surgery still held when the
/// pipeline and the chain are torn down completes after teardown and its
/// late publication into the dropped receiver is harmless — no panic, no
/// stranded handle visible to the caller.
#[test]
fn a_pending_edit_torn_down_before_completion_makes_a_late_publication_harmless() {
    let Some(mut live) = LiveEqChain::start(ClipProtection::Soft) else {
        return;
    };
    let hold = Arc::new(ProbeEditHold::new());
    live.chain.inject_probe_edit_hold(Arc::clone(&hold));
    let releaser = std::thread::spawn({
        let hold = Arc::clone(&hold);
        move || {
            assert!(hold.wait_until_entered(Duration::from_secs(5)));
            std::thread::sleep(LIMITER_PROBE_ENGAGE_TIMEOUT + Duration::from_millis(100));
            hold.release();
        }
    });

    let parked = live
        .chain
        .swap_clip_protection_under_block_probe(ClipProtection::Off);
    assert_eq!(parked, None);
    assert!(live.chain.has_pending_limiter_edit());

    // Tear the pipeline down and drop the chain — transaction included —
    // while the surgery is still executing on the streaming thread.
    live.stop();
    let LiveEqChain { chain, .. } = live;
    drop(chain);

    // The callback finishes after teardown and its publication finds the
    // receiver gone; joining proves the streaming thread unwound cleanly.
    releaser.join().expect("the callback must survive teardown");
}
