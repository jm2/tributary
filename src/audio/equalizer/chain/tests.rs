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
pub(super) fn bin_requires_plugins() -> bool {
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
pub(super) fn assert_links_eq_through_clipper(bin: &gst::Bin) {
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
pub(super) fn assert_links_eq_directly_to_post_convert(bin: &gst::Bin) {
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

/// Regression (review finding G, PR #220): the limiter insert/remove is
/// delivered as the documented dynamic in-bin edit under a blocking pad
/// probe on the `equalizer-10bands` src pad. The blocking probe pins the
/// EQ output while the graph is rewired and is removed only over the
/// validated topology, so the edit lands without pausing the pipeline.
#[test]
fn dynamic_limiter_swap_rewires_the_graph_under_the_blocking_probe() {
    if !bin_requires_plugins() {
        return;
    }
    let mut chain = EqChain::build(&EqSettings {
        enabled: true,
        ..EqSettings::default()
    })
    .expect("eq-bin builds");

    // Off → Soft: dynamic insert.
    assert_eq!(
        chain.swap_clip_protection_under_block_probe(ClipProtection::Soft),
        Some(true)
    );
    assert!(chain.clip_protection_installed());
    assert_links_eq_through_clipper(&chain.bin);
    assert!(chain.bin.by_name("clipper").is_some());

    // Soft → Off: dynamic removal.
    assert_eq!(
        chain.swap_clip_protection_under_block_probe(ClipProtection::Off),
        Some(true)
    );
    assert!(!chain.clip_protection_installed());
    assert!(chain.bin.by_name("clipper").is_none());
    assert_links_eq_directly_to_post_convert(&chain.bin);

    // Every validated edit removed its probe again: no blocking probe is
    // left installed and the chain is not wedged (contract: the probe is
    // uninstalled only over a validated topology).
    let eq_src = chain.eq.static_pad("src").expect("eq src pad");
    assert!(
        !eq_src.is_blocked(),
        "a validated dynamic edit must uninstall its blocking probe"
    );
    assert!(!chain.topology_wedged());
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

/// Regression (operator F1, PR #220): a failed direct relink in the
/// limiter-removal surgery must put the owned handle back and leave
/// the working limiter path routed — `clip_protection_installed`
/// must never report `Off` while a `clipper` is still in the bin.
/// The injected fault fires once, so the retried removal lands.
#[test]
fn failed_limiter_removal_keeps_the_handle_and_a_routed_graph() {
    if !bin_requires_plugins() {
        return;
    }
    let mut chain = EqChain::build(&EqSettings {
        enabled: true,
        clip_protection: ClipProtection::Soft,
        ..EqSettings::default()
    })
    .expect("eq-bin builds");
    assert!(chain.clip_protection_installed());

    // Refuse the direct relink once at the real surgery boundary.
    chain.inject_limiter_remove_fault(LimiterRemoveFault::DirectRelinkBlocked);
    assert!(
        !chain.set_clip_protection(ClipProtection::Off),
        "a failed removal must report that Off was not installed"
    );
    assert!(
        chain.clip_protection_installed(),
        "the owned handle must be restored when the limiter stays routed"
    );
    assert!(
        chain.bin.by_name("clipper").is_some(),
        "the limiter must still be inside the bin"
    );
    assert_links_eq_through_clipper(&chain.bin);

    // The fault fired once: the retried surgery uses the real graph.
    assert!(
        chain.set_clip_protection(ClipProtection::Off),
        "the retried removal must succeed"
    );
    assert!(!chain.clip_protection_installed());
    assert!(chain.bin.by_name("clipper").is_none());
    assert_links_eq_directly_to_post_convert(&chain.bin);
}

/// Regression (operator F1, restoration arm): when the direct relink
/// *and* the limiter-path restoration both fail, the surgery must not
/// resume an unlinked `eq` source. It tears the partial links down,
/// removes the limiter, forces the direct path, and reports the
/// requested `Off` as installed — a truthful, working no-limiter
/// topology a later enable can still edit.
#[test]
fn doubly_failed_limiter_removal_leaves_no_unlinked_graph() {
    if !bin_requires_plugins() {
        return;
    }
    let mut chain = EqChain::build(&EqSettings {
        enabled: true,
        clip_protection: ClipProtection::Soft,
        ..EqSettings::default()
    })
    .expect("eq-bin builds");

    chain.inject_limiter_remove_fault(LimiterRemoveFault::DirectRelinkAndRestore);
    assert!(
        chain.set_clip_protection(ClipProtection::Off),
        "the forced direct path must satisfy the requested removal"
    );
    assert!(!chain.clip_protection_installed());
    assert!(chain.bin.by_name("clipper").is_none());
    assert_links_eq_directly_to_post_convert(&chain.bin);

    // The fault fired once: a later enable still edits the graph.
    assert!(chain.set_clip_protection(ClipProtection::Soft));
    assert_links_eq_through_clipper(&chain.bin);
    assert!(chain.set_clip_protection(ClipProtection::Off));
    assert_links_eq_directly_to_post_convert(&chain.bin);
}

/// Regression (review finding H, PR #220): when the limiter surgery and
/// its rollback cannot validate a linked topology, the blocking-probe
/// callback must keep the probe installed — the `equalizer-10bands` src
/// pad stays blocked so no buffer is ever pushed across the unlinked pad
/// (no `GST_FLOW_NOT_LINKED`) — and the chain must report itself wedged so
/// the caller's pause/relink fallback cannot fabricate a successful toggle
/// over the unlinked graph. The wedged pipeline is retired by the ordinary
/// eq-bin-originated bus seam, never resumed.
#[test]
fn unlinked_rollback_keeps_the_blocking_probe_installed_and_wedges_the_chain() {
    if !bin_requires_plugins() {
        return;
    }
    let mut chain = EqChain::build(&EqSettings {
        enabled: true,
        clip_protection: ClipProtection::Soft,
        ..EqSettings::default()
    })
    .expect("eq-bin builds");
    assert!(chain.clip_protection_installed());
    let eq_src = chain.eq.static_pad("src").expect("eq src pad");
    assert!(
        !eq_src.is_blocked(),
        "no probe is installed before the edit"
    );

    // Refuse the direct relink, the limiter-path restoration, and the final
    // forced direct link: every rollback the surgery can attempt fails, so
    // the eq src pad is left with no linked peer.
    chain.inject_limiter_remove_fault(LimiterRemoveFault::EveryLinkBlocked);
    assert_eq!(
        chain.swap_clip_protection_under_block_probe(ClipProtection::Off),
        Some(false),
        "a wedged rollback must report the requested toggle as unmet"
    );

    // The probe stayed installed (the pad is still blocked), the graph is
    // unlinked, and the chain records the wedge.
    assert!(
        eq_src.is_blocked(),
        "the callback must keep the blocking probe installed when no linked topology exists"
    );
    assert!(
        eq_src.peer().is_none(),
        "the failed edit and rollback must leave the eq src pad unlinked"
    );
    assert!(!chain.clip_protection_installed());
    assert!(chain.topology_wedged());

    // A wedged chain refuses every further topology edit: the pause/relink
    // fallback cannot report a successful toggle over the still-unlinked
    // graph, and it must not disturb the installed probe.
    assert!(
        !chain.set_clip_protection(ClipProtection::Off),
        "a wedged chain must not fabricate a successful toggle"
    );
    assert!(chain.topology_wedged());
    assert!(eq_src.is_blocked());
    assert!(eq_src.peer().is_none());
}

/// Regression (review finding H, PR #220): a wedge must post the explicit
/// error diagnostic the contract requires, sourced from inside `eq-bin`,
/// so the ordinary eq-bin-originated bus seam retires the wedged pipeline
/// instead of resuming it.
#[test]
fn unlinked_rollback_posts_an_eq_bin_originated_error_on_the_bus() {
    if !bin_requires_plugins() {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let mut chain = EqChain::build(&EqSettings {
        enabled: true,
        clip_protection: ClipProtection::Soft,
        ..EqSettings::default()
    })
    .expect("eq-bin builds");
    pipeline.add(&chain.bin).expect("bin added to a pipeline");
    let bus = pipeline.bus().expect("pipeline bus");

    chain.inject_limiter_remove_fault(LimiterRemoveFault::EveryLinkBlocked);
    assert_eq!(
        chain.swap_clip_protection_under_block_probe(ClipProtection::Off),
        Some(false)
    );

    let message = bus
        .timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Error])
        .expect("the wedge must post an error diagnostic on the bus");
    let source = message.src().expect("an error message has a source");
    assert_eq!(
        source.name().as_str(),
        "eq-bin",
        "the diagnostic must originate inside eq-bin so the retirement seam fires"
    );
    assert!(chain.topology_wedged());
    let _ = pipeline.set_state(gst::State::Null);
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

/// Regression (review thread: preset gains must land at an actual
/// buffer boundary): on a chain that is not in `NULL`, the batch is
/// delivered through the idle probe on the bin's sink ghost pad
/// instead of a bare property storm. On an idle bin (READY, no data
/// flow) the probe fires synchronously, so the batch has landed by
/// the time the call returns — and returned `Remove`, so the next
/// batch lands through a fresh probe too.
#[test]
fn band_transaction_on_a_running_bin_lands_through_the_sink_pad_probe() {
    if !bin_requires_plugins() {
        return;
    }
    let chain = EqChain::build(&EqSettings {
        enabled: true,
        ..EqSettings::default()
    })
    .expect("eq-bin builds");
    chain
        .bin
        .set_state(gst::State::Ready)
        .expect("a no-data bin reaches READY synchronously");
    assert_ne!(chain.bin.current_state(), gst::State::Null);

    let first = EqSettings {
        enabled: true,
        preset: Preset::Custom,
        preamp_db: -3.0,
        bands_db: [-2.0, -1.0, 0.0, 1.0, 2.0, 3.0, 2.0, 1.0, 0.0, -1.0],
        clip_protection: ClipProtection::Off,
    };
    chain.apply_band_transaction(&first);
    let preamp = chain.bin.by_name("eq-preamp").unwrap();
    assert!(
        (preamp.property::<f64>("volume") - EqSettings::preamp_db_to_factor(-3.0)).abs() < 1e-6
    );
    let eq = chain.bin.by_name("eq").unwrap();
    for (index, expected) in first.bands_db.iter().enumerate() {
        let written: f64 = eq.property(&format!("band{index}"));
        assert!((written - *expected).abs() < 1e-9, "band{index}");
    }

    // The probe removed itself: a second batch lands just as fully.
    let second = EqSettings {
        preamp_db: 6.0,
        bands_db: [0.5; 10],
        ..first
    };
    chain.apply_band_transaction(&second);
    assert!((preamp.property::<f64>("volume") - EqSettings::preamp_db_to_factor(6.0)).abs() < 1e-6);
    for (index, expected) in second.bands_db.iter().enumerate() {
        let written: f64 = eq.property(&format!("band{index}"));
        assert!((written - *expected).abs() < 1e-9, "band{index}");
    }
    chain.bin.set_state(gst::State::Null).expect("bin to NULL");
}
