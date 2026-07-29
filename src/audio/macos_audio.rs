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
#[cfg(any(target_os = "macos", test))]
const SINK_REOPEN_ATTEMPT_LIMIT: usize = 3;

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReopenFollowUp {
    None,
    ReplayLatest,
    RetryFailure { attempts_remaining: usize },
    Exhausted,
}

#[cfg(any(target_os = "macos", test))]
fn reopen_follow_up(
    generation_changed: bool,
    reopen_succeeded: bool,
    attempts_remaining: usize,
) -> ReopenFollowUp {
    if generation_changed {
        ReopenFollowUp::ReplayLatest
    } else if reopen_succeeded {
        ReopenFollowUp::None
    } else if attempts_remaining > 1 {
        ReopenFollowUp::RetryFailure {
            attempts_remaining: attempts_remaining - 1,
        }
    } else {
        ReopenFollowUp::Exhausted
    }
}

#[cfg(any(target_os = "macos", test))]
fn reopen_attempts_for_generation(
    requested_generation: u64,
    observed_generation: u64,
    attempts_remaining: usize,
) -> usize {
    if requested_generation == observed_generation {
        attempts_remaining
    } else {
        SINK_REOPEN_ATTEMPT_LIMIT
    }
}

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
    let channel_caps = guarded_channel_caps(&native)?;
    assemble_sink_bin(native, channel_caps)
}

#[cfg(target_os = "macos")]
fn guarded_channel_caps(native: &gst::Element) -> Result<gst::Caps, glib::BoolError> {
    let native_pad = native
        .static_pad("sink")
        .ok_or_else(|| glib::bool_error!("osxaudiosink has no sink pad"))?;
    let channel_caps = cap_raw_audio_channels(native_pad.pad_template_caps());
    if !has_stereo_channel_cap(&channel_caps) {
        return Err(glib::bool_error!(
            "osxaudiosink did not advertise channel-bearing raw-audio caps"
        ));
    }
    Ok(channel_caps)
}

#[cfg(target_os = "macos")]
fn assemble_sink_bin(
    native: gst::Element,
    channel_caps: gst::Caps,
) -> Result<ConfiguredSinkBin, glib::BoolError> {
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

    let gate = install_route_pads(&bin, &identity)?;
    Ok(ConfiguredSinkBin { bin, native, gate })
}

#[cfg(target_os = "macos")]
fn install_route_pads(
    bin: &gst::Bin,
    identity: &gst::Element,
) -> Result<gst::Pad, glib::BoolError> {
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
    Ok(gate)
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
#[path = "macos_audio_native.rs"]
mod native;

#[cfg(test)]
#[path = "macos_audio_tests.rs"]
mod tests;
