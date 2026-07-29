//! macOS system-default audio routing.
//!
//! GStreamer's `osxaudiosink` resolves the CoreAudio default only while the
//! sink opens. It does not currently observe
//! `kAudioHardwarePropertyDefaultOutputDevice`, so changing the system output
//! can leave a live pipeline attached to the previous device. Tributary owns
//! that missing route boundary: CoreAudio notifications are coalesced off the
//! native callback thread, then the existing sink is reopened on the GLib main
//! context without rebuilding playbin or changing the playback session.
//!
//! Every sink is wrapped behind a persistent two-channel `capsfilter` before
//! playbin sees it. Reopening the same guarded sink makes that workaround an
//! invariant across output changes instead of relying on playbin's incidental
//! `element-setup` ordering.

#![cfg_attr(test, allow(dead_code))]

#[cfg(any(target_os = "macos", test))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use gst::prelude::*;
use gstreamer as gst;
#[cfg(target_os = "macos")]
use gtk::glib;

const OSX_AUDIO_FACTORY: &str = "osxaudiosink";
#[cfg(target_os = "macos")]
const CHANNEL_FILTER_FACTORY: &str = "capsfilter";

/// Coalesces native route notifications across the interval in which the
/// GStreamer gate is installed and its main-context reopen is still pending.
#[cfg(any(target_os = "macos", test))]
struct ReopenCoordinator {
    generation: AtomicU64,
    pending: AtomicBool,
}

#[cfg(any(target_os = "macos", test))]
impl ReopenCoordinator {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            pending: AtomicBool::new(false),
        }
    }

    fn record(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn claim(&self) -> bool {
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn snapshot(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn finish(&self, applied_generation: u64) -> bool {
        self.pending.store(false, Ordering::Release);
        self.generation.load(Ordering::Acquire) != applied_generation
    }

    fn abandon(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "macos")]
type PendingRouteProbe = Arc<Mutex<Option<(gst::Pad, gst::PadProbeId)>>>;

#[cfg(target_os = "macos")]
fn remove_pending_route_probe(pending: &PendingRouteProbe) {
    let probe = match pending.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    if let Some((gate, probe_id)) = probe {
        gate.remove_probe(probe_id);
    }
}

#[cfg(any(target_os = "macos", test))]
fn route_gate_probe_type() -> gst::PadProbeType {
    // IDLE schedules as soon as the current push drains. BLOCK_DOWNSTREAM
    // holds later buffers/lists/downstream events, while QUERY_DOWNSTREAM
    // closes the query gap in that aggregate mask. Upstream RECONFIGURE is
    // deliberately not matched and can still leave the reopened sink.
    gst::PadProbeType::IDLE
        | gst::PadProbeType::BLOCK_DOWNSTREAM
        | gst::PadProbeType::QUERY_DOWNSTREAM
}

/// Retains the explicit native sink and, on macOS, its CoreAudio listener.
#[cfg(target_os = "macos")]
pub(super) struct MacosAudioRoute {
    _sink_bin: gst::Bin,
    listener: Option<DefaultOutputListener>,
    alive: Arc<AtomicBool>,
    pending_probe: PendingRouteProbe,
}

#[cfg(target_os = "macos")]
impl MacosAudioRoute {
    pub(super) fn install(playbin: &gst::Element) -> Option<Self> {
        let sink = match configured_sink_bin() {
            Ok(sink) => sink,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Could not construct the app-owned macOS audio route; using guarded automatic-sink fallback"
                );
                install_playbin_channel_cap_fallback(playbin);
                return None;
            }
        };
        // The explicit sink is configured before playbin can open or
        // negotiate it. It remains the same object across route changes.
        playbin.set_property("audio-sink", &sink.bin);

        let alive = Arc::new(AtomicBool::new(true));
        let pending_probe = Arc::new(Mutex::new(None));
        let listener = DefaultOutputListener::install(
            playbin,
            &sink.native,
            &sink.gate,
            Arc::clone(&alive),
            Arc::clone(&pending_probe),
        )
        .map_err(|status| {
            tracing::warn!(
                status,
                "Could not observe the macOS default audio output; the configured sink remains usable"
            );
        });

        Some(Self {
            _sink_bin: sink.bin,
            listener: listener.ok(),
            alive,
            pending_probe,
        })
    }
}

#[cfg(target_os = "macos")]
struct ConfiguredSinkBin {
    bin: gst::Bin,
    native: gst::Element,
    /// App-owned stable pad upstream of the native sink. Blocking this pad
    /// avoids contending with `GstAudioBaseSink`'s own sink-pad stream lock
    /// while the native element moves to `NULL`.
    gate: gst::Pad,
}

#[cfg(target_os = "macos")]
fn configured_sink_bin() -> Result<ConfiguredSinkBin, glib::BoolError> {
    let native = gst::ElementFactory::make(OSX_AUDIO_FACTORY)
        .build()
        .map_err(|_| glib::bool_error!("could not create osxaudiosink"))?;
    let native_pad = native
        .static_pad("sink")
        .ok_or_else(|| glib::bool_error!("osxaudiosink has no sink pad"))?;
    let channel_caps = cap_raw_audio_channels(native_pad.pad_template_caps());
    if !has_stereo_channel_cap(&channel_caps) {
        return Err(glib::bool_error!(
            "osxaudiosink did not advertise channel-bearing raw-audio caps"
        ));
    }

    let channel_filter = gst::ElementFactory::make(CHANNEL_FILTER_FACTORY)
        .build()
        .map_err(|_| glib::bool_error!("could not create macOS channel guard"))?;
    channel_filter.set_property("caps", &channel_caps);
    let identity = gst::ElementFactory::make("identity")
        .build()
        .map_err(|_| glib::bool_error!("could not create macOS audio route gate"))?;
    let bin = gst::Bin::with_name("tributary-macos-audio-route");
    bin.add_many([&identity, &channel_filter, &native])?;
    identity.link(&channel_filter)?;
    channel_filter.link(&native)?;

    let identity_sink = identity
        .static_pad("sink")
        .ok_or_else(|| glib::bool_error!("macOS audio route gate has no sink pad"))?;
    let ghost = gst::GhostPad::builder_with_target(&identity_sink)?
        .name("sink")
        .build();
    ghost.set_active(true)?;
    bin.add_pad(&ghost)?;

    let gate = identity
        .static_pad("src")
        .ok_or_else(|| glib::bool_error!("macOS audio route gate has no source pad"))?;
    Ok(ConfiguredSinkBin { bin, native, gate })
}

#[cfg(target_os = "macos")]
impl Drop for MacosAudioRoute {
    fn drop(&mut self) {
        // Make already-queued main-context work inert before closing/removing
        // the native listener.
        self.alive.store(false, Ordering::Release);
        drop(self.listener.take());
        remove_pending_route_probe(&self.pending_probe);
    }
}

/// Restrict raw structures by intersection, never replacement. This preserves
/// every native rate/format/feature and cannot widen a mono-only device to
/// stereo. Compressed structures pass through unchanged.
fn cap_raw_audio_channels(caps: gst::Caps) -> gst::Caps {
    if caps.is_any() {
        return caps;
    }

    let raw_stereo_limit = gst::Structure::builder("audio/x-raw")
        .field("channels", gst::IntRange::new(1, 2))
        .build();
    let mut capped = gst::Caps::new_empty();
    for (structure, features) in caps.iter_with_features() {
        let restricted = if structure.name().as_str() == "audio/x-raw" {
            structure.intersect(&raw_stereo_limit)
        } else {
            Some(structure.to_owned())
        };
        if let Some(structure) = restricted {
            capped
                .make_mut()
                .append_structure_full(structure, Some(features.to_owned()));
        }
    }
    capped
}

fn has_stereo_channel_cap(caps: &gst::CapsRef) -> bool {
    let raw = gst::Caps::builder("audio/x-raw").build();
    let raw_over_stereo = gst::Caps::builder("audio/x-raw")
        .field("channels", gst::IntRange::new(3, i32::MAX))
        .build();
    caps.can_intersect(&raw) && !caps.can_intersect(&raw_over_stereo)
}

/// Degraded source-build fallback for a runtime missing one explicit-route
/// element. PULL query probes run after the pad's real query function, so the
/// native sink still supplies device-specific caps before they are narrowed.
#[cfg(target_os = "macos")]
fn install_channel_cap_after_native_query(sink: &gst::Element) -> bool {
    if sink
        .factory()
        .is_none_or(|factory| factory.name() != OSX_AUDIO_FACTORY)
    {
        return false;
    }
    let Some(pad) = sink.static_pad("sink") else {
        return false;
    };
    let probe = pad.add_probe(
        gst::PadProbeType::QUERY_DOWNSTREAM | gst::PadProbeType::PULL,
        |_pad, info| {
            let Some(query) = info.query_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            if let gst::QueryViewMut::Caps(caps_query) = query.view_mut() {
                if let Some(result) = caps_query.result_owned() {
                    caps_query.set_result(&cap_raw_audio_channels(result));
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
    probe.is_some()
}

#[cfg(target_os = "macos")]
fn install_playbin_channel_cap_fallback(playbin: &gst::Element) {
    playbin.connect("element-setup", false, |args| {
        let element = args.get(1)?.get::<gst::Element>().ok()?;
        if install_channel_cap_after_native_query(&element) {
            tracing::info!("macOS: installed the post-query stereo guard on fallback osxaudiosink");
        }
        None
    });
}

#[cfg(target_os = "macos")]
use native::DefaultOutputListener;

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::{null, NonNull};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    use block2::RcBlock;
    use gst::prelude::*;
    use gstreamer as gst;
    use gtk::glib;
    use objc2_core_audio::{
        kAudioHardwareNoError, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        kAudioObjectUnknown, AudioObjectAddPropertyListenerBlock, AudioObjectGetPropertyData,
        AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListenerBlock,
    };

    use super::{
        remove_pending_route_probe, route_gate_probe_type, PendingRouteProbe, ReopenCoordinator,
    };

    type ListenerBlock = RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>) + 'static>;

    const DEFAULT_OUTPUT_RETRY_COUNT: usize = 5;
    const DEFAULT_OUTPUT_RETRY_DELAY: Duration = Duration::from_millis(50);

    pub(super) struct DefaultOutputListener {
        address: AudioObjectPropertyAddress,
        callback: ListenerBlock,
        signal_tx: async_channel::Sender<()>,
    }

    impl DefaultOutputListener {
        pub(super) fn install(
            playbin: &gst::Element,
            sink: &gst::Element,
            gate: &gst::Pad,
            alive: Arc<AtomicBool>,
            pending_probe: PendingRouteProbe,
        ) -> Result<Self, i32> {
            let (signal_tx, signal_rx) = async_channel::bounded(1);
            let callback_tx = signal_tx.clone();
            let callback: ListenerBlock = RcBlock::new(
                move |_address_count: u32, _addresses: NonNull<AudioObjectPropertyAddress>| {
                    // This block can run on CoreAudio's private thread. Its
                    // only capture is a thread-safe bounded sender; it never
                    // touches GStreamer/GTK state, logs, waits, or panics.
                    let _ = callback_tx.try_send(());
                },
            );
            let mut address = default_output_address();

            // SAFETY: CoreAudio consumes the address synchronously and copies
            // the valid heap block. The returned listener retains the exact
            // address/block/queue tuple for removal. Passing no dispatch queue
            // permits CoreAudio's callback thread, which is safe because the
            // block only performs a nonblocking send through a Send + Sync
            // channel.
            // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
            let status = unsafe {
                AudioObjectAddPropertyListenerBlock(
                    system_object(),
                    NonNull::from(&mut address),
                    None,
                    RcBlock::as_ptr(&callback),
                )
            };
            if status != kAudioHardwareNoError {
                signal_tx.close();
                return Err(status);
            }

            let playbin = playbin.downgrade();
            let sink = sink.downgrade();
            let gate = gate.downgrade();
            let coordinator = Arc::new(ReopenCoordinator::new());
            glib::MainContext::default().spawn_local(async move {
                while signal_rx.recv().await.is_ok() {
                    if !alive.load(Ordering::Acquire) {
                        break;
                    }

                    let mut output_available = false;
                    for attempt in 0..DEFAULT_OUTPUT_RETRY_COUNT {
                        output_available = default_output_is_available();
                        if output_available {
                            break;
                        }
                        if attempt + 1 < DEFAULT_OUTPUT_RETRY_COUNT {
                            glib::timeout_future(DEFAULT_OUTPUT_RETRY_DELAY).await;
                        }
                    }

                    if !output_available {
                        tracing::warn!(
                            "macOS default audio output is temporarily unavailable; retaining the current sink"
                        );
                        continue;
                    }

                    coordinator.record();
                    let (Some(playbin), Some(sink), Some(gate)) =
                        (playbin.upgrade(), sink.upgrade(), gate.upgrade())
                    else {
                        break;
                    };
                    request_sink_reopen(
                        &playbin,
                        &sink,
                        &gate,
                        Arc::clone(&alive),
                        Arc::clone(&coordinator),
                        Arc::clone(&pending_probe),
                    );
                }
            });

            Ok(Self {
                address,
                callback,
                signal_tx,
            })
        }
    }

    impl Drop for DefaultOutputListener {
        fn drop(&mut self) {
            // Closing first makes a late invocation or failed removal
            // harmless: CoreAudio's retained block owns only a closed sender.
            self.signal_tx.close();
            // SAFETY: This exactly matches registration: same system object,
            // address, no dispatch queue, and the still-live block pointer.
            // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
            let status = unsafe {
                AudioObjectRemovePropertyListenerBlock(
                    system_object(),
                    NonNull::from(&mut self.address),
                    None,
                    RcBlock::as_ptr(&self.callback),
                )
            };
            if status != kAudioHardwareNoError {
                tracing::warn!(
                    status,
                    "Could not remove the macOS default-output listener; its closed callback remains inert"
                );
            }
        }
    }

    const fn default_output_address() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    const fn system_object() -> AudioObjectID {
        kAudioObjectSystemObject
    }

    fn default_output_is_available() -> bool {
        let mut address = default_output_address();
        let mut size = u32::try_from(size_of::<AudioObjectID>())
            .expect("AudioObjectID size fits CoreAudio's UInt32");
        let mut device = kAudioObjectUnknown;

        // SAFETY: Every pointer refers to a live, correctly sized local for
        // the duration of this synchronous CoreAudio property query.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        let status = unsafe {
            AudioObjectGetPropertyData(
                system_object(),
                NonNull::from(&mut address),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::from(&mut device).cast::<c_void>(),
            )
        };
        status == kAudioHardwareNoError
            && size
                == u32::try_from(size_of::<AudioObjectID>())
                    .expect("AudioObjectID size fits CoreAudio's UInt32")
            && device != kAudioObjectUnknown
    }

    pub(super) fn request_sink_reopen(
        playbin: &gst::Element,
        sink: &gst::Element,
        gate: &gst::Pad,
        alive: Arc<AtomicBool>,
        coordinator: Arc<ReopenCoordinator>,
        pending_probe: PendingRouteProbe,
    ) {
        if !alive.load(Ordering::Acquire) || !coordinator.claim() {
            return;
        }
        let playbin = playbin.downgrade();
        let sink = sink.downgrade();
        let dispatched = Arc::new(AtomicBool::new(false));
        let dispatched_from_probe = Arc::clone(&dispatched);
        let pending_from_probe = Arc::clone(&pending_probe);

        let probe = gate.add_probe(route_gate_probe_type(), move |gate, _info| {
            if dispatched_from_probe.swap(true, Ordering::AcqRel) {
                return gst::PadProbeReturn::Ok;
            }
            let gate = gate.clone();
            let playbin = playbin.clone();
            let sink = sink.clone();
            let alive = Arc::clone(&alive);
            let coordinator = Arc::clone(&coordinator);
            let pending_probe = Arc::clone(&pending_from_probe);

            // A probe may execute on a streaming thread. The combined
            // IDLE/BLOCK/query mask remains flow-blocking after this callback
            // returns; perform every state mutation on the default GLib main
            // context, then remove the tracked probe to release the stream.
            glib::idle_add_once(move || {
                let mut applied_generation = None;
                if alive.load(Ordering::Acquire) {
                    if let (Some(playbin), Some(sink)) = (playbin.upgrade(), sink.upgrade()) {
                        let generation = coordinator.snapshot();
                        reopen_sink_on_main_context(&playbin, &sink);
                        applied_generation = Some(generation);
                    }
                }
                remove_pending_route_probe(&pending_probe);

                // A second notification may arrive after the first request
                // installed its gate. Reopening resolves the latest system
                // default, and a generation mismatch closes the remaining
                // race—even for disconnect/reconnect of the same endpoint.
                let replay =
                    applied_generation.is_some_and(|generation| coordinator.finish(generation));
                if alive.load(Ordering::Acquire) && replay {
                    if let (Some(playbin), Some(sink)) = (playbin.upgrade(), sink.upgrade()) {
                        request_sink_reopen(
                            &playbin,
                            &sink,
                            &gate,
                            Arc::clone(&alive),
                            Arc::clone(&coordinator),
                            Arc::clone(&pending_probe),
                        );
                    }
                } else if applied_generation.is_none() {
                    coordinator.abandon();
                }
            });

            gst::PadProbeReturn::Ok
        });

        if let Some(probe_id) = probe {
            let displaced = match pending_probe.lock() {
                Ok(mut pending) => pending.replace((gate.clone(), probe_id)),
                Err(poisoned) => poisoned.into_inner().replace((gate.clone(), probe_id)),
            };
            if let Some((old_gate, old_probe_id)) = displaced {
                old_gate.remove_probe(old_probe_id);
                tracing::warn!("Replaced an unexpected pending macOS audio-route gate");
            }
        } else {
            coordinator.abandon();
            tracing::warn!("Could not block the macOS audio sink for a device reopen");
        }
    }

    fn reopen_sink_on_main_context(playbin: &gst::Element, sink: &gst::Element) {
        const CURRENT_DEFAULT_DEVICE: i32 = 0;

        let (_, current, pending) = sink.state(gst::ClockTime::ZERO);
        if current == gst::State::Null && pending == gst::State::VoidPending {
            // Keep the sink unpinned. Its next normal open resolves whatever
            // CoreAudio default is current at that time.
            sink.set_property("device", CURRENT_DEFAULT_DEVICE);
            return;
        }

        let volume = playbin.property::<f64>("volume");
        if sink.set_state(gst::State::Null).is_err() {
            tracing::warn!("Could not close the previous macOS audio output");
            return;
        }
        // `AudioObjectID` is an opaque UInt32, while osxaudiosink's explicit
        // `device` property accepts only 0..G_MAXINT. Its zero sentinel asks
        // GStreamer to resolve the full-width current CoreAudio default and
        // also selects the newest default if another change raced this reopen.
        sink.set_property("device", CURRENT_DEFAULT_DEVICE);
        if let Err(error) = sink.sync_state_with_parent() {
            tracing::warn!(
                error = %error,
                "Could not reopen the macOS audio sink on the current system output"
            );
            return;
        }

        // The new endpoint can advertise different formats/channels. Ask
        // upstream conversion to negotiate again while the pad is still
        // blocked; the channel `capsfilter` remains in the retained sink bin.
        if let Some(sink_pad) = sink.static_pad("sink") {
            if !sink_pad.push_event(gst::event::Reconfigure::new()) {
                tracing::warn!(
                    "Could not request macOS audio renegotiation after an output change"
                );
            }
        }
        playbin.set_property("volume", volume);
        tracing::info!("macOS system audio output changed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::prelude::*;

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

    #[test]
    fn persistent_capsfilter_preserves_and_refreshes_downstream_constraints() {
        gst::init().expect("initialize GStreamer");
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
        let guard_pad = guard.static_pad("sink").expect("channel guard sink pad");
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
    fn route_gate_stays_flow_blocking_until_removed() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        gst::init().expect("initialize GStreamer");
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

        pipeline
            .set_state(gst::State::Playing)
            .expect("start route-gate pipeline");
        let startup_deadline = Instant::now() + Duration::from_secs(2);
        while delivered.load(AtomicOrdering::Acquire) < 5 && Instant::now() < startup_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            delivered.load(AtomicOrdering::Acquire) >= 5,
            "live test pipeline did not begin delivering buffers"
        );

        let gate = gate_element.static_pad("src").expect("route gate pad");
        let dispatched = Arc::new(AtomicBool::new(false));
        let dispatched_from_gate = Arc::clone(&dispatched);
        let route_probe = gate
            .add_probe(route_gate_probe_type(), move |_pad, _info| {
                dispatched_from_gate.store(true, Ordering::Release);
                gst::PadProbeReturn::Ok
            })
            .expect("install route gate");
        let block_deadline = Instant::now() + Duration::from_secs(2);
        while (!dispatched.load(Ordering::Acquire) || !gate.is_blocking())
            && Instant::now() < block_deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(dispatched.load(Ordering::Acquire));
        assert!(gate.is_blocking(), "route gate did not hold the stream");

        let held_count = delivered.load(AtomicOrdering::Acquire);
        std::thread::sleep(Duration::from_millis(75));
        assert_eq!(
            delivered.load(AtomicOrdering::Acquire),
            held_count,
            "buffers crossed the route gate while native sink mutation was pending"
        );

        gate.remove_probe(route_probe);
        let release_deadline = Instant::now() + Duration::from_secs(2);
        while delivered.load(AtomicOrdering::Acquire) == held_count
            && Instant::now() < release_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            delivered.load(AtomicOrdering::Acquire) > held_count,
            "removing the route gate did not resume the stream"
        );

        sink_pad.remove_probe(delivery_probe);
        pipeline
            .set_state(gst::State::Null)
            .expect("stop route-gate pipeline");
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
}
