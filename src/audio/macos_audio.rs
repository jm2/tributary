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
//! Every sink is decorated before playbin sees it with the existing
//! two-channel caps workaround. Reopening the same decorated sink makes that
//! workaround an invariant across output changes instead of relying on
//! playbin's incidental `element-setup` ordering.

#![cfg_attr(test, allow(dead_code))]

#[cfg(any(target_os = "macos", test))]
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;
#[cfg(target_os = "macos")]
use gtk::glib;

const OSX_AUDIO_FACTORY: &str = "osxaudiosink";

/// Coalesces native route notifications across the interval in which the
/// GStreamer gate is installed and its main-context reopen is still pending.
#[cfg(any(target_os = "macos", test))]
struct ReopenCoordinator {
    latest_device: AtomicI32,
    generation: AtomicU64,
    pending: AtomicBool,
}

#[cfg(any(target_os = "macos", test))]
impl ReopenCoordinator {
    const fn new() -> Self {
        Self {
            latest_device: AtomicI32::new(0),
            generation: AtomicU64::new(0),
            pending: AtomicBool::new(false),
        }
    }

    fn record(&self, device_id: i32) {
        self.latest_device.store(device_id, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn claim(&self) -> bool {
        self.pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn snapshot(&self) -> (i32, u64) {
        loop {
            let before = self.generation.load(Ordering::Acquire);
            let device_id = self.latest_device.load(Ordering::Acquire);
            let after = self.generation.load(Ordering::Acquire);
            if before == after {
                return (device_id, after);
            }
        }
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
                    "osxaudiosink unavailable; macOS system-output changes use GStreamer's fallback behavior"
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
    if !install_channel_cap_on_sink(&native) {
        return Err(glib::bool_error!("osxaudiosink has no usable sink pad"));
    }
    let identity = gst::ElementFactory::make("identity")
        .build()
        .map_err(|_| glib::bool_error!("could not create macOS audio route gate"))?;
    let bin = gst::Bin::with_name("tributary-macos-audio-route");
    bin.add_many([&identity, &native])?;
    identity.link(&native)?;

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

/// Install the multichannel-negotiation workaround directly on one native
/// sink before it can be opened.
///
/// Some CoreAudio devices advertise up to eight channels without usable
/// channel positions. GStreamer's converter can then fixate a stereo stream to
/// that maximum and fail `not-negotiated`. Restricting the queried raw-audio
/// channel range to one or two preserves the source layout.
fn install_channel_cap_on_sink(sink: &gst::Element) -> bool {
    if sink
        .factory()
        .is_none_or(|factory| factory.name() != OSX_AUDIO_FACTORY)
    {
        return false;
    }
    let Some(pad) = sink.static_pad("sink") else {
        return false;
    };

    pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |pad, info| {
        let Some(query) = info.query_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        if query.type_() != gst::QueryType::Caps {
            return gst::PadProbeReturn::Ok;
        }

        // Let the native sink answer first, then narrow only its raw-audio
        // channel field. All other fields and caps features remain unchanged.
        let parent = pad.parent_element();
        let handled = parent
            .as_ref()
            .is_some_and(|element| gst::Pad::query_default(pad, Some(element), query));
        if !handled {
            return gst::PadProbeReturn::Ok;
        }

        if let gst::QueryViewMut::Caps(caps_query) = query.view_mut() {
            if let Some(result) = caps_query.result_owned() {
                caps_query.set_result(&cap_raw_audio_channels(result));
            }
        }

        gst::PadProbeReturn::Handled
    });

    tracing::info!("macOS: installed the stereo channel-cap invariant on osxaudiosink");
    true
}

/// Retain the old automatic-sink behavior if the explicit native sink cannot
/// be constructed. This still applies the channel cap if playbin later creates
/// an `osxaudiosink`, but route following is unavailable in that degraded path.
#[cfg(target_os = "macos")]
fn install_playbin_channel_cap_fallback(playbin: &gst::Element) {
    playbin.connect("element-setup", false, |args| {
        let element = args.get(1)?.get::<gst::Element>().ok()?;
        install_channel_cap_on_sink(&element);
        None
    });
}

fn cap_raw_audio_channels(mut caps: gst::Caps) -> gst::Caps {
    if caps.is_any() {
        return caps;
    }

    for structure in caps.make_mut().iter_mut() {
        if structure.name().as_str() == "audio/x-raw" && structure.has_field("channels") {
            structure.set("channels", gst::IntRange::new(1, 2));
        }
    }
    caps
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

    use super::{remove_pending_route_probe, PendingRouteProbe, ReopenCoordinator};

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

                    let mut usable_device = None;
                    for attempt in 0..DEFAULT_OUTPUT_RETRY_COUNT {
                        usable_device = current_default_output();
                        if usable_device.is_some() {
                            break;
                        }
                        if attempt + 1 < DEFAULT_OUTPUT_RETRY_COUNT {
                            glib::timeout_future(DEFAULT_OUTPUT_RETRY_DELAY).await;
                        }
                    }

                    let Some(device_id) = usable_device else {
                        tracing::warn!(
                            "macOS default audio output is temporarily unavailable; retaining the current sink"
                        );
                        continue;
                    };

                    coordinator.record(device_id);
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

    fn current_default_output() -> Option<i32> {
        let mut address = default_output_address();
        let mut size = u32::try_from(size_of::<AudioObjectID>())
            .expect("AudioObjectID size fits CoreAudio's UInt32");
        let mut device = kAudioObjectUnknown;

        // SAFETY: Every pointer refers to a live, correctly sized local for
        // the duration of this synchronous CoreAudio property query.
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
        let valid = status == kAudioHardwareNoError
            && size == u32::try_from(size_of::<AudioObjectID>()).ok()?
            && device != kAudioObjectUnknown;
        if !valid {
            return None;
        }
        i32::try_from(device).ok()
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

        let probe = gate.add_probe(gst::PadProbeType::IDLE, move |gate, _info| {
            if dispatched_from_probe.swap(true, Ordering::AcqRel) {
                return gst::PadProbeReturn::Ok;
            }
            let gate = gate.clone();
            let playbin = playbin.clone();
            let sink = sink.clone();
            let alive = Arc::clone(&alive);
            let coordinator = Arc::clone(&coordinator);
            let pending_probe = Arc::clone(&pending_from_probe);

            // A probe may execute on a streaming thread. Keep it blocking and
            // perform every state mutation on the default GLib main context.
            glib::idle_add_once(move || {
                let mut applied_generation = None;
                if alive.load(Ordering::Acquire) {
                    if let (Some(playbin), Some(sink)) = (playbin.upgrade(), sink.upgrade()) {
                        let (device_id, generation) = coordinator.snapshot();
                        reopen_sink_on_main_context(&playbin, &sink, device_id);
                        applied_generation = Some(generation);
                    }
                }
                remove_pending_route_probe(&pending_probe);

                // A second notification may arrive after the first request
                // installed its gate. The latest ID is used above, and a
                // generation mismatch closes the remaining race—even when a
                // disconnect/reconnect reports the same numeric device ID.
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

    fn reopen_sink_on_main_context(playbin: &gst::Element, sink: &gst::Element, device_id: i32) {
        let (_, current, pending) = sink.state(gst::ClockTime::ZERO);
        if current == gst::State::Null && pending == gst::State::VoidPending {
            // Pin the notification's current default without opening an idle
            // sink. The next normal playbin state change uses this endpoint.
            sink.set_property("device", device_id);
            return;
        }

        let volume = playbin.property::<f64>("volume");
        if sink.set_state(gst::State::Null).is_err() {
            tracing::warn!("Could not close the previous macOS audio output");
            return;
        }
        // osxaudiosink clears this during READY→NULL. Pin the valid default
        // snapshot that triggered this reopen; a subsequent CoreAudio change
        // queues another bounded reopen.
        sink.set_property("device", device_id);
        if let Err(error) = sink.sync_state_with_parent() {
            tracing::warn!(
                error = %error,
                "Could not reopen the macOS audio sink on the current system output"
            );
            return;
        }

        // The new endpoint can advertise different formats/channels. Ask
        // upstream conversion to negotiate again while the pad is still
        // blocked; the channel-cap probe remains installed on this sink.
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
    fn caps_channel_cap_handles_any_empty_and_missing_channel_caps() {
        gst::init().expect("initialize GStreamer");
        assert!(cap_raw_audio_channels(gst::Caps::new_any()).is_any());
        assert!(cap_raw_audio_channels(gst::Caps::new_empty()).is_empty());

        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .build();
        let capped = cap_raw_audio_channels(caps);
        assert!(!capped
            .structure(0)
            .expect("raw structure")
            .has_field("channels"));
    }

    #[test]
    fn reopen_coordinator_coalesces_and_replays_changes_during_a_pending_reopen() {
        let coordinator = ReopenCoordinator::new();
        coordinator.record(17);
        assert!(coordinator.claim());
        let (first_device, first_generation) = coordinator.snapshot();
        assert_eq!(first_device, 17);

        coordinator.record(23);
        assert!(!coordinator.claim());
        let (latest_device, latest_generation) = coordinator.snapshot();
        assert_eq!(latest_device, 23);
        assert!(latest_generation > first_generation);
        assert!(coordinator.finish(first_generation));
        assert!(coordinator.claim());
        assert!(!coordinator.finish(latest_generation));
    }

    #[test]
    fn reopen_coordinator_replays_same_device_reconnect_notifications() {
        let coordinator = ReopenCoordinator::new();
        coordinator.record(17);
        assert!(coordinator.claim());
        let (_, first_generation) = coordinator.snapshot();
        coordinator.record(17);
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
                .native
                .static_pad("sink")
                .expect("osxaudiosink pad")
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
}
