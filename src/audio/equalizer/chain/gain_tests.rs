//! Regressions for the gain-delta delivery split (refinery round 4,
//! PR 220 A2 — *Live-reconfiguration boundary*): a single changed
//! property is written directly with no probe transaction and nothing
//! else rewritten; a multi-property delta lands as exactly one
//! probe-delivered batch; a no-op delta writes nothing. Moved verbatim
//! from `tests.rs` under the file-length gate.

use super::tests::bin_requires_plugins;
use super::*;

/// Install notify counters on every band property plus the preamp
/// `volume`, keyed by property name. GObject emits `notify` on every
/// property *set* — even when the value is unchanged — so a zero count
/// means the property was genuinely never written.
#[allow(clippy::type_complexity)] // test-local counter shape, used verbatim below
fn gain_notify_counters(
    eq: &gst::Element,
    preamp: &gst::Element,
) -> (
    Vec<(String, std::rc::Rc<std::cell::Cell<u32>>)>,
    std::rc::Rc<std::cell::Cell<u32>>,
) {
    use std::cell::Cell;
    use std::rc::Rc;

    let mut counters: Vec<(String, Rc<Cell<u32>>)> = Vec::new();
    for index in 0..10 {
        let count = Rc::new(Cell::new(0u32));
        let hits = Rc::clone(&count);
        eq.connect_notify_local(Some(&format!("band{index}")), move |_, _| {
            hits.set(hits.get() + 1);
        });
        counters.push((format!("band{index}"), count));
    }
    let preamp_count = Rc::new(Cell::new(0u32));
    let hits = Rc::clone(&preamp_count);
    preamp.connect_notify_local(Some("volume"), move |_, _| {
        hits.set(hits.get() + 1);
    });
    (counters, preamp_count)
}

/// Regression (refinery round 4, PR 220 A2 — *Live-reconfiguration
/// boundary*): a gain delta that changes a single band must bypass the
/// batch transaction entirely — the property is written directly on the
/// caller's thread and lands immediately, and nothing else on the chain
/// (other bands, preamp) is rewritten.
#[test]
fn single_band_gain_delta_writes_one_property_directly() {
    if !bin_requires_plugins() {
        return;
    }
    let previous = EqSettings {
        enabled: true,
        ..EqSettings::default()
    };
    let chain = EqChain::build(&previous).expect("eq-bin builds");
    let eq = chain.bin.by_name("eq").unwrap();
    let preamp = chain.bin.by_name("eq-preamp").unwrap();
    let (counters, preamp_count) = gain_notify_counters(&eq, &preamp);

    let mut next = previous;
    next.bands_db[3] = -3.0;
    assert!(chain.apply_gain_delta(&previous, &next));

    assert_eq!(
        counters[3].1.get(),
        1,
        "band3 must be written exactly once, directly"
    );
    for (name, count) in &counters {
        if name != "band3" {
            assert_eq!(count.get(), 0, "{name} must not be rewritten");
        }
    }
    assert_eq!(preamp_count.get(), 0, "preamp must not be rewritten");
    let written: f64 = eq.property("band3");
    assert!((written - (-3.0)).abs() < 1e-9);
}

/// Regression (refinery round 4, PR 220 A2): a preamp-only gain delta
/// writes only the preamp `volume` stage — no band is rewritten.
#[test]
fn preamp_only_gain_delta_writes_one_property_directly() {
    if !bin_requires_plugins() {
        return;
    }
    let previous = EqSettings {
        enabled: true,
        ..EqSettings::default()
    };
    let chain = EqChain::build(&previous).expect("eq-bin builds");
    let eq = chain.bin.by_name("eq").unwrap();
    let preamp = chain.bin.by_name("eq-preamp").unwrap();
    let (counters, preamp_count) = gain_notify_counters(&eq, &preamp);

    let mut next = previous;
    next.preamp_db = -6.0;
    assert!(chain.apply_gain_delta(&previous, &next));

    assert_eq!(
        preamp_count.get(),
        1,
        "the preamp volume must be written exactly once, directly"
    );
    for (name, count) in &counters {
        assert_eq!(count.get(), 0, "{name} must not be rewritten");
    }
    assert!(
        (preamp.property::<f64>("volume") - EqSettings::preamp_db_to_factor(-6.0)).abs() < 1e-6
    );
}

/// Regression (refinery round 4, PR 220 A2): a gain delta touching two
/// or more properties still routes through the batch transaction, and on
/// an idle (READY, no data flow) bin the idle probe fires synchronously,
/// so the batch has landed by the time the call returns. The batch is a
/// full write-set — every gain property is rewritten inside the one
/// probe-delivered batch — so the assertion here is that exactly the
/// *changed* properties carry their new values and the whole set landed
/// as one write-set, not that untouched properties were spared (that is
/// the single-property direct path's job, covered by the two tests
/// above).
#[test]
fn multi_band_gain_delta_writes_exactly_the_changed_properties() {
    if !bin_requires_plugins() {
        return;
    }
    let previous = EqSettings {
        enabled: true,
        ..EqSettings::default()
    };
    let chain = EqChain::build(&previous).expect("eq-bin builds");
    chain
        .bin
        .set_state(gst::State::Ready)
        .expect("a no-data bin reaches READY synchronously");
    assert_ne!(chain.bin.current_state(), gst::State::Null);
    let eq = chain.bin.by_name("eq").unwrap();
    let preamp = chain.bin.by_name("eq-preamp").unwrap();
    let (counters, preamp_count) = gain_notify_counters(&eq, &preamp);

    let mut next = previous;
    next.preamp_db = -6.0;
    next.bands_db[1] = -2.0;
    next.bands_db[5] = 4.0;
    assert!(chain.apply_gain_delta(&previous, &next));

    // One batch write-set: every property written exactly once, by the
    // single probe-delivered transaction.
    assert_eq!(
        preamp_count.get(),
        1,
        "the preamp volume must be rewritten exactly once"
    );
    for (name, count) in &counters {
        assert_eq!(count.get(), 1, "{name} write count within the one batch");
    }
    assert!(
        (preamp.property::<f64>("volume") - EqSettings::preamp_db_to_factor(-6.0)).abs() < 1e-6
    );
    for (index, expected) in next.bands_db.iter().enumerate() {
        let written: f64 = eq.property(&format!("band{index}"));
        assert!((written - *expected).abs() < 1e-9, "band{index}");
    }
    chain.bin.set_state(gst::State::Null).expect("bin to NULL");
}

/// Regression (refinery round 4, PR 220 A2): a gain delta with no
/// changed property writes nothing and reports nothing to push — the
/// aggregate settings-changed event must not fire for a no-op apply.
#[test]
fn unchanged_gain_delta_writes_nothing() {
    if !bin_requires_plugins() {
        return;
    }
    let previous = EqSettings {
        enabled: true,
        ..EqSettings::default()
    };
    let chain = EqChain::build(&previous).expect("eq-bin builds");
    let eq = chain.bin.by_name("eq").unwrap();
    let preamp = chain.bin.by_name("eq-preamp").unwrap();
    let (counters, preamp_count) = gain_notify_counters(&eq, &preamp);

    assert!(!chain.apply_gain_delta(&previous, &previous));
    for (name, count) in &counters {
        assert_eq!(count.get(), 0, "{name} must not be rewritten");
    }
    assert_eq!(preamp_count.get(), 0, "preamp must not be rewritten");
}
