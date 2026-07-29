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
        let mut address = default_output_address();
        let (signal_tx, signal_rx, callback) = register_default_output_listener(&mut address)?;
        spawn_default_output_worker(signal_rx, playbin, sink, gate, alive, pending_probe);
        Ok(Self {
            address,
            callback,
            signal_tx,
        })
    }
}

fn register_default_output_listener(
    address: &mut AudioObjectPropertyAddress,
) -> Result<
    (
        async_channel::Sender<()>,
        async_channel::Receiver<()>,
        ListenerBlock,
    ),
    i32,
> {
    let (signal_tx, signal_rx) = async_channel::bounded(1);
    let callback_tx = signal_tx.clone();
    let callback: ListenerBlock = RcBlock::new(
        move |_address_count: u32, _addresses: NonNull<AudioObjectPropertyAddress>| {
            // CoreAudio can invoke this on a private thread. Keep the
            // callback nonblocking and isolated from GStreamer/GTK.
            let _ = callback_tx.try_send(());
        },
    );

    // SAFETY: CoreAudio consumes the address synchronously and copies the
    // valid heap block. The listener retains this exact address/block/
    // queue tuple for removal. No dispatch queue is safe because the block
    // performs only a nonblocking send through a Send + Sync channel.
    // nosemgrep
    let status = unsafe {
        AudioObjectAddPropertyListenerBlock(
            system_object(),
            NonNull::from(address),
            None,
            RcBlock::as_ptr(&callback),
        )
    };
    if status != kAudioHardwareNoError {
        signal_tx.close();
        return Err(status);
    }
    Ok((signal_tx, signal_rx, callback))
}

fn spawn_default_output_worker(
    signal_rx: async_channel::Receiver<()>,
    playbin: &gst::Element,
    sink: &gst::Element,
    gate: &gst::Pad,
    alive: Arc<AtomicBool>,
    pending_probe: PendingRouteProbe,
) {
    let playbin = playbin.downgrade();
    let sink = sink.downgrade();
    let gate = gate.downgrade();
    let coordinator = Arc::new(ReopenCoordinator::new());
    glib::MainContext::default().spawn_local(async move {
        while signal_rx.recv().await.is_ok() {
            if !alive.load(Ordering::Acquire) {
                break;
            }
            if !wait_for_default_output().await {
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
}

async fn wait_for_default_output() -> bool {
    for attempt in 0..DEFAULT_OUTPUT_RETRY_COUNT {
        if default_output_is_available() {
            return true;
        }
        if attempt + 1 < DEFAULT_OUTPUT_RETRY_COUNT {
            glib::timeout_future(DEFAULT_OUTPUT_RETRY_DELAY).await;
        }
    }
    false
}

impl Drop for DefaultOutputListener {
    fn drop(&mut self) {
        // Closing first makes a late invocation or failed removal
        // harmless: CoreAudio's retained block owns only a closed sender.
        self.signal_tx.close();
        // SAFETY: This exactly matches registration: same system object,
        // address, no dispatch queue, and the still-live block pointer.
        // nosemgrep
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

fn system_object() -> AudioObjectID {
    AudioObjectID::try_from(kAudioObjectSystemObject)
        .expect("CoreAudio's system-object constant fits AudioObjectID")
}

fn default_output_is_available() -> bool {
    let mut address = default_output_address();
    let mut size = u32::try_from(size_of::<AudioObjectID>())
        .expect("AudioObjectID size fits CoreAudio's UInt32");
    let mut device = kAudioObjectUnknown;

    // SAFETY: Every pointer refers to a live, correctly sized local for
    // the duration of this synchronous CoreAudio property query.
    // nosemgrep
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
    let coordinator_from_probe = Arc::clone(&coordinator);

    let probe = gate.add_probe(route_gate_probe_type(), move |gate, _info| {
        if dispatched_from_probe.swap(true, Ordering::AcqRel) {
            return gst::PadProbeReturn::Ok;
        }
        schedule_sink_reopen(
            gate.clone(),
            playbin.clone(),
            sink.clone(),
            Arc::clone(&alive),
            Arc::clone(&coordinator_from_probe),
            Arc::clone(&pending_from_probe),
        );

        gst::PadProbeReturn::Ok
    });
    track_route_probe(probe, gate, &pending_probe, &coordinator);
}

fn track_route_probe(
    probe: Option<gst::PadProbeId>,
    gate: &gst::Pad,
    pending_probe: &PendingRouteProbe,
    coordinator: &ReopenCoordinator,
) {
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

fn schedule_sink_reopen(
    gate: gst::Pad,
    playbin: glib::WeakRef<gst::Element>,
    sink: glib::WeakRef<gst::Element>,
    alive: Arc<AtomicBool>,
    coordinator: Arc<ReopenCoordinator>,
    pending_probe: PendingRouteProbe,
) {
    // A probe may execute on a streaming thread. The combined
    // IDLE/BLOCK/query mask remains flow-blocking after this callback
    // returns; perform every state mutation on the default GLib main
    // context, then remove the tracked probe to release the stream.
    glib::idle_add_once(move || {
        finish_sink_reopen_on_main_context(gate, playbin, sink, alive, coordinator, pending_probe);
    });
}

fn finish_sink_reopen_on_main_context(
    gate: gst::Pad,
    playbin: glib::WeakRef<gst::Element>,
    sink: glib::WeakRef<gst::Element>,
    alive: Arc<AtomicBool>,
    coordinator: Arc<ReopenCoordinator>,
    pending_probe: PendingRouteProbe,
) {
    let mut applied_generation = None;
    if alive.load(Ordering::Acquire) {
        if let (Some(playbin), Some(sink)) = (playbin.upgrade(), sink.upgrade()) {
            let generation = coordinator.snapshot();
            reopen_sink_on_main_context(&playbin, &sink);
            applied_generation = Some(generation);
        }
    }
    remove_pending_route_probe(&pending_probe);

    // A notification arriving after this gate was installed must replay.
    // Resolving the current default handles same-device reconnects too.
    let replay = applied_generation.is_some_and(|generation| coordinator.finish(generation));
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
            tracing::warn!("Could not request macOS audio renegotiation after an output change");
        }
    }
    playbin.set_property("volume", volume);
    tracing::info!("macOS system audio output changed");
}
