use super::*;
#[cfg(not(target_os = "macos"))]
use gst::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn caps_channel_cap_preserves_other_fields_structures_and_features() {
    gst::init().expect("initialize GStreamer");
    let caps = gst::Caps::builder_full()
        .structure_with_features(
            gst::Structure::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", gst::IntRange::new(8_000, 192_000))
                .field("channels", gst::IntRange::new(1, 8))
                .build(),
            gst::CapsFeatures::new(["memory:TributaryTest"]),
        )
        .structure(
            gst::Structure::builder("audio/x-compressed")
                .field("channels", 8_i32)
                .build(),
        )
        .build();

    let capped = cap_raw_audio_channels(caps);
    let raw = capped.structure(0).expect("raw structure");
    assert_eq!(raw.get::<&str>("format"), Ok("F32LE"));
    assert_eq!(
        raw.get::<gst::IntRange<i32>>("rate"),
        Ok(gst::IntRange::new(8_000, 192_000))
    );
    assert_eq!(
        raw.get::<gst::IntRange<i32>>("channels"),
        Ok(gst::IntRange::new(1, 2))
    );
    let features = capped.features(0).expect("raw caps features");
    assert_eq!(features.size(), 1);
    assert!(features.contains("memory:TributaryTest"));
    assert_eq!(
        capped
            .structure(1)
            .expect("compressed structure")
            .get::<i32>("channels"),
        Ok(8)
    );
}

#[test]
fn caps_channel_cap_handles_any_empty_and_constrains_missing_channels() {
    gst::init().expect("initialize GStreamer");
    assert!(cap_raw_audio_channels(gst::Caps::new_any()).is_any());
    assert!(cap_raw_audio_channels(gst::Caps::new_empty()).is_empty());

    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .build();
    let capped = cap_raw_audio_channels(caps);
    assert_eq!(
        capped
            .structure(0)
            .expect("raw structure")
            .get::<gst::IntRange<i32>>("channels"),
        Ok(gst::IntRange::new(1, 2))
    );
}

#[test]
fn caps_channel_cap_intersects_without_widening_native_support() {
    gst::init().expect("initialize GStreamer");
    let mono = gst::Caps::builder("audio/x-raw")
        .field("channels", 1_i32)
        .build();
    let capped_mono = cap_raw_audio_channels(mono);
    assert_eq!(
        capped_mono
            .structure(0)
            .expect("mono raw structure")
            .get::<i32>("channels"),
        Ok(1)
    );

    let surround_only = gst::Caps::builder("audio/x-raw")
        .field("channels", gst::IntRange::new(3, 8))
        .build();
    assert!(cap_raw_audio_channels(surround_only).is_empty());
}

fn persistent_capsfilter_fixture() -> (gst::Element, gst::Element, gst::Element, gst::Pad) {
    let native_template = gst::Caps::builder_full()
        .structure(
            gst::Structure::builder("audio/x-raw")
                .field("rate", gst::IntRange::new(8_000, 192_000))
                .field("channels", gst::IntRange::new(1, 8))
                .build(),
        )
        .structure(
            gst::Structure::builder("audio/x-ac3")
                .field("framed", true)
                .build(),
        )
        .build();
    let guard = gst::ElementFactory::make("capsfilter")
        .build()
        .expect("channel capsfilter");
    guard.set_property("caps", cap_raw_audio_channels(native_template));
    let device = gst::ElementFactory::make("capsfilter")
        .build()
        .expect("simulated device capsfilter");
    let sink = gst::ElementFactory::make("fakesink")
        .build()
        .expect("query sink");
    guard.link(&device).expect("link channel guard");
    device.link(&sink).expect("link simulated device");
    let guard_pad = guard.static_pad("sink").expect("channel guard sink pad");
    (guard, device, sink, guard_pad)
}

fn assert_initial_device_constraints(device: &gst::Element, guard_pad: &gst::Pad) {
    let first_device_caps = gst::Caps::builder_full()
        .structure(
            gst::Structure::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", 48_000_i32)
                .field("channels", gst::IntRange::new(1, 8))
                .build(),
        )
        .structure(
            gst::Structure::builder("audio/x-ac3")
                .field("framed", true)
                .build(),
        )
        .build();
    device.set_property("caps", &first_device_caps);
    let first = guard_pad.query_caps(None);
    let first_raw = first
        .iter()
        .find(|structure| structure.name().as_str() == "audio/x-raw")
        .expect("first device raw caps");
    assert_eq!(first_raw.get::<&str>("format"), Ok("F32LE"));
    assert_eq!(first_raw.get::<i32>("rate"), Ok(48_000));
    assert_eq!(
        first_raw.get::<gst::IntRange<i32>>("channels"),
        Ok(gst::IntRange::new(1, 2))
    );
    assert!(first
        .iter()
        .any(|structure| structure.name().as_str() == "audio/x-ac3"));
}

fn assert_caps_acceptance_constraints(guard_pad: &gst::Pad) {
    let eight_channels = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", 48_000_i32)
        .field("channels", 8_i32)
        .build();
    let stereo = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", 48_000_i32)
        .field("channels", 2_i32)
        .build();
    let ac3 = gst::Caps::builder("audio/x-ac3")
        .field("framed", true)
        .build();
    assert!(!guard_pad.query_accept_caps(&eight_channels));
    assert!(guard_pad.query_accept_caps(&stereo));
    assert!(guard_pad.query_accept_caps(&ac3));
}

fn assert_refreshed_device_constraints(device: &gst::Element, guard_pad: &gst::Pad) {
    let second_device_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", 44_100_i32)
        .field("channels", 1_i32)
        .build();
    device.set_property("caps", &second_device_caps);
    let second = guard_pad.query_caps(None);
    let second_raw = second.structure(0).expect("second device raw caps");
    assert_eq!(second_raw.get::<&str>("format"), Ok("S16LE"));
    assert_eq!(second_raw.get::<i32>("rate"), Ok(44_100));
    assert_eq!(second_raw.get::<i32>("channels"), Ok(1));
    assert!(!second
        .iter()
        .any(|structure| structure.name().as_str() == "audio/x-ac3"));
}

#[test]
fn persistent_capsfilter_preserves_and_refreshes_downstream_constraints() {
    gst::init().expect("initialize GStreamer");
    let (_guard, device, _sink, guard_pad) = persistent_capsfilter_fixture();
    assert_initial_device_constraints(&device, &guard_pad);
    assert_caps_acceptance_constraints(&guard_pad);
    assert_refreshed_device_constraints(&device, &guard_pad);
}

struct RouteGateFixture {
    pipeline: gst::Pipeline,
    gate: gst::Pad,
    sink_pad: gst::Pad,
    delivered: Arc<AtomicUsize>,
    delivery_probe: gst::PadProbeId,
}

fn route_gate_fixture() -> RouteGateFixture {
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .expect("live audio source");
    let gate_element = gst::ElementFactory::make("identity")
        .build()
        .expect("route gate");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("audio sink");
    pipeline
        .add_many([&source, &gate_element, &sink])
        .expect("assemble route-gate pipeline");
    source.link(&gate_element).expect("link source to gate");
    gate_element.link(&sink).expect("link gate to sink");

    let delivered = Arc::new(AtomicUsize::new(0));
    let delivered_from_probe = Arc::clone(&delivered);
    let sink_pad = sink.static_pad("sink").expect("sink pad");
    let delivery_probe = sink_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            delivered_from_probe.fetch_add(1, AtomicOrdering::AcqRel);
            gst::PadProbeReturn::Ok
        })
        .expect("delivery counter probe");
    let gate = gate_element.static_pad("src").expect("route gate pad");

    RouteGateFixture {
        pipeline,
        gate,
        sink_pad,
        delivered,
        delivery_probe,
    }
}

fn start_route_gate_pipeline(fixture: &RouteGateFixture) {
    fixture
        .pipeline
        .set_state(gst::State::Playing)
        .expect("start route-gate pipeline");
    let startup_deadline = Instant::now() + Duration::from_secs(2);
    while fixture.delivered.load(AtomicOrdering::Acquire) < 5 && Instant::now() < startup_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fixture.delivered.load(AtomicOrdering::Acquire) >= 5,
        "live test pipeline did not begin delivering buffers"
    );
}

fn install_route_gate(fixture: &RouteGateFixture) -> gst::PadProbeId {
    let dispatched = Arc::new(AtomicBool::new(false));
    let dispatched_from_gate = Arc::clone(&dispatched);
    let route_probe = fixture
        .gate
        .add_probe(route_gate_probe_type(), move |_pad, _info| {
            dispatched_from_gate.store(true, Ordering::Release);
            gst::PadProbeReturn::Ok
        })
        .expect("install route gate");
    let block_deadline = Instant::now() + Duration::from_secs(2);
    while (!dispatched.load(Ordering::Acquire) || !fixture.gate.is_blocking())
        && Instant::now() < block_deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(dispatched.load(Ordering::Acquire));
    assert!(
        fixture.gate.is_blocking(),
        "route gate did not hold the stream"
    );
    route_probe
}

fn assert_route_gate_holds(fixture: &RouteGateFixture) -> usize {
    let held_count = fixture.delivered.load(AtomicOrdering::Acquire);
    std::thread::sleep(Duration::from_millis(75));
    assert_eq!(
        fixture.delivered.load(AtomicOrdering::Acquire),
        held_count,
        "buffers crossed the route gate while native sink mutation was pending"
    );
    held_count
}

fn remove_route_gate_and_assert_resume(
    fixture: &RouteGateFixture,
    route_probe: gst::PadProbeId,
    held_count: usize,
) {
    fixture.gate.remove_probe(route_probe);
    let release_deadline = Instant::now() + Duration::from_secs(2);
    while fixture.delivered.load(AtomicOrdering::Acquire) == held_count
        && Instant::now() < release_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fixture.delivered.load(AtomicOrdering::Acquire) > held_count,
        "removing the route gate did not resume the stream"
    );
}

fn stop_route_gate_pipeline(fixture: RouteGateFixture) {
    fixture.sink_pad.remove_probe(fixture.delivery_probe);
    fixture
        .pipeline
        .set_state(gst::State::Null)
        .expect("stop route-gate pipeline");
}

#[test]
fn route_gate_stays_flow_blocking_until_removed() {
    gst::init().expect("initialize GStreamer");
    let fixture = route_gate_fixture();
    start_route_gate_pipeline(&fixture);
    let route_probe = install_route_gate(&fixture);
    let held_count = assert_route_gate_holds(&fixture);
    remove_route_gate_and_assert_resume(&fixture, route_probe, held_count);
    stop_route_gate_pipeline(fixture);
}

#[test]
fn reopen_coordinator_coalesces_and_replays_changes_during_a_pending_reopen() {
    let coordinator = ReopenCoordinator::new();
    coordinator.record();
    assert!(coordinator.claim());
    let first_generation = coordinator.snapshot();

    coordinator.record();
    assert!(!coordinator.claim());
    let latest_generation = coordinator.snapshot();
    assert!(latest_generation > first_generation);
    assert!(coordinator.finish(first_generation));
    assert!(coordinator.claim());
    assert!(!coordinator.finish(latest_generation));
}

#[test]
fn reopen_coordinator_replays_reconnect_notifications() {
    let coordinator = ReopenCoordinator::new();
    coordinator.record();
    assert!(coordinator.claim());
    let first_generation = coordinator.snapshot();
    coordinator.record();
    assert!(coordinator.finish(first_generation));
}

#[test]
fn reopen_failures_retry_boundedly_and_new_generations_take_priority() {
    assert_eq!(
        reopen_follow_up(false, true, SINK_REOPEN_ATTEMPT_LIMIT),
        ReopenFollowUp::None
    );
    assert_eq!(
        reopen_follow_up(false, false, SINK_REOPEN_ATTEMPT_LIMIT),
        ReopenFollowUp::RetryFailure {
            attempts_remaining: 2
        }
    );
    assert_eq!(
        reopen_follow_up(false, false, 2),
        ReopenFollowUp::RetryFailure {
            attempts_remaining: 1
        }
    );
    assert_eq!(reopen_follow_up(false, false, 1), ReopenFollowUp::Exhausted);
    assert_eq!(
        reopen_follow_up(true, false, 1),
        ReopenFollowUp::ReplayLatest
    );
    assert_eq!(
        reopen_follow_up(true, true, 1),
        ReopenFollowUp::ReplayLatest
    );
    assert_eq!(reopen_attempts_for_generation(7, 7, 1), 1);
    assert_eq!(
        reopen_attempts_for_generation(7, 8, 1),
        SINK_REOPEN_ATTEMPT_LIMIT
    );
}

#[cfg(target_os = "macos")]
#[test]
fn every_route_wrapper_is_complete_and_capped_before_open() {
    gst::init().expect("initialize GStreamer");
    for _ in 0..2 {
        let route = configured_sink_bin().expect("construct app-owned route");
        assert!(route.bin.static_pad("sink").is_some());
        let caps = route
            .bin
            .static_pad("sink")
            .expect("app-owned route pad")
            .query_caps(None);
        let mut saw_raw_channels = false;
        for structure in caps.iter() {
            if structure.name().as_str() == "audio/x-raw" && structure.has_field("channels") {
                saw_raw_channels = true;
                assert_eq!(
                    structure.get::<gst::IntRange<i32>>("channels"),
                    Ok(gst::IntRange::new(1, 2))
                );
            }
        }
        assert!(
            saw_raw_channels,
            "osxaudiosink must expose channel-bearing raw-audio caps"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn automatic_sink_fallback_caps_the_completed_native_query() {
    gst::init().expect("initialize GStreamer");
    let native = gst::ElementFactory::make(OSX_AUDIO_FACTORY)
        .build()
        .expect("construct fallback osxaudiosink");
    assert!(install_channel_cap_after_native_query(&native));
    let caps = native
        .static_pad("sink")
        .expect("fallback osxaudiosink pad")
        .query_caps(None);
    assert!(has_stereo_channel_cap(&caps));
}
