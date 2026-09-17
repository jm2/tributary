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
/// thread, and return the swap result plus the thread the held callback ran
/// on — so the test can prove the edit was dispatched asynchronously.
fn swap_across_engagement_window(
    live: &mut LiveEqChain,
    hold: &Arc<ProbeEditHold>,
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
    let result = live
        .chain
        .swap_clip_protection_under_block_probe(ClipProtection::Off);
    releaser.join().expect("releaser thread");
    (result, hold.callback_thread())
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
    let (result, callback_thread) = swap_across_engagement_window(&mut live, &hold);

    assert_ne!(
        callback_thread,
        Some(caller_thread),
        "the probe callback must run on the streaming thread for this regression"
    );
    assert_eq!(
        result,
        Some(false),
        "the caller must observe the valid rollback, not cancel the executing edit"
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
    let (result, callback_thread) = swap_across_engagement_window(&mut live, &hold);

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
