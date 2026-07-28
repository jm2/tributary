//! Windows system-default audio endpoint tracking.
//!
//! GStreamer's `wasapi2sink` can change devices without rebuilding the
//! playback pipeline, but the application must feed default-device changes
//! from `GstDeviceMonitor` back into the sink.  Keeping one explicit sink also
//! keeps its per-stream volume cache alive across endpoint replacement.

// Compile the integration during test builds on every platform so CI catches
// GStreamer Rust API drift even when no Windows runner is available.
#![cfg_attr(test, allow(dead_code))]

#[cfg(any(target_os = "windows", test))]
use std::cell::Cell;
#[cfg(any(target_os = "windows", test))]
use std::rc::Rc;

#[cfg(any(target_os = "windows", test))]
use gst::prelude::*;
#[cfg(any(target_os = "windows", test))]
use gstreamer as gst;
#[cfg(any(target_os = "windows", test))]
use gtk::glib;
#[cfg(any(target_os = "windows", test))]
use tracing::{info, warn};

#[cfg(any(target_os = "windows", test))]
const WASAPI2_FACTORY: &str = "wasapi2sink";
const WASAPI2_API: &str = "wasapi2";

#[cfg(any(target_os = "windows", test))]
pub(super) struct WindowsAudioRoute {
    monitor: gst::DeviceMonitor,
    _monitor_watch: gst::bus::BusWatchGuard,
}

#[cfg(any(target_os = "windows", test))]
impl WindowsAudioRoute {
    pub(super) fn install(
        playbin: &gst::Element,
        volume: Rc<Cell<f64>>,
        recovery_claimed: Rc<Cell<bool>>,
    ) -> Option<Self> {
        let sink = match gst::ElementFactory::make(WASAPI2_FACTORY).build() {
            Ok(sink) => sink,
            Err(error) => {
                warn!(
                    error = %error,
                    "wasapi2sink unavailable; system output changes use GStreamer's fallback behavior"
                );
                return None;
            }
        };
        if !configure_wasapi2_sink(&sink) {
            warn!(
                "wasapi2sink lacks dynamic-device recovery; system output changes use GStreamer's fallback behavior"
            );
            return None;
        }

        let monitor = gst::DeviceMonitor::new();
        if monitor.add_filter(Some("Audio/Sink"), None).is_none() {
            warn!(
                "No Windows audio device provider available; system output changes use GStreamer's fallback behavior"
            );
            return None;
        }

        let sink_for_watch = sink.clone();
        let playbin_weak = playbin.downgrade();
        let recovery_claimed_for_watch = Rc::clone(&recovery_claimed);
        let monitor_watch = match monitor.bus().add_watch_local(move |_bus, message| {
            let device = match message.view() {
                gst::MessageView::DeviceAdded(added) => Some(added.device()),
                gst::MessageView::DeviceChanged(changed) => Some(changed.device()),
                _ => None,
            };
            let Some(device) = device else {
                return glib::ControlFlow::Continue;
            };
            let Some(endpoint_id) = default_wasapi2_endpoint_id(device.properties().as_deref())
            else {
                return glib::ControlFlow::Continue;
            };

            recovery_claimed_for_watch.set(false);
            sink_for_watch.set_property("device", endpoint_id.as_str());
            if let Some(playbin) = playbin_weak.upgrade() {
                reapply_cached_volume(&playbin, volume.get());
            }
            info!("Windows system audio output changed");
            glib::ControlFlow::Continue
        }) {
            Ok(watch) => watch,
            Err(error) => {
                warn!(
                    error = %error,
                    "Could not watch Windows audio devices; system output changes use GStreamer's fallback behavior"
                );
                return None;
            }
        };

        if let Err(error) = monitor.start() {
            warn!(
                error = %error,
                "Could not start Windows audio monitoring; system output changes use GStreamer's fallback behavior"
            );
            return None;
        }

        if let Some(endpoint_id) = monitor
            .devices()
            .iter()
            .find_map(|device| default_wasapi2_endpoint_id(device.properties().as_deref()))
        {
            sink.set_property("device", endpoint_id.as_str());
        }

        playbin.set_property("audio-sink", &sink);
        Some(Self {
            monitor,
            _monitor_watch: monitor_watch,
        })
    }
}

#[cfg(any(target_os = "windows", test))]
impl Drop for WindowsAudioRoute {
    fn drop(&mut self) {
        self.monitor.stop();
    }
}

/// Enable the non-terminal device-failure path introduced with GStreamer 1.28.
///
/// Feature detection keeps source builds with an older system GStreamer on the
/// automatic sink path instead of assuming live replacement is available.
#[cfg(any(target_os = "windows", test))]
pub(super) fn configure_wasapi2_sink(sink: &gst::Element) -> bool {
    let is_wasapi2 = sink
        .factory()
        .is_some_and(|factory| factory.name() == WASAPI2_FACTORY);
    let supports_live_device_switch = sink
        .find_property("device")
        .is_some_and(|property| property.flags().contains(glib::ParamFlags::WRITABLE));
    if !is_wasapi2
        || !supports_live_device_switch
        || sink.find_property("continue-on-error").is_none()
    {
        return false;
    }

    sink.set_property("continue-on-error", true);
    sink.property::<bool>("continue-on-error")
}

/// Retry an invalidated endpoint after `continue-on-error` turns the native
/// device failure into a warning. At most one reconnect is attempted before a
/// new load or device-monitor event resets the claim. Without that latch, an
/// absent endpoint can turn each failed reconnect into another warning and an
/// unbounded retry loop.
#[cfg(any(target_os = "windows", test))]
pub(super) fn recover_warning(
    message: &gst::MessageRef,
    playbin: &gst::Element,
    volume: f64,
    recovery_claimed: &Cell<bool>,
) -> bool {
    let Some(sink) = message.src().and_then(|source| {
        source.downcast_ref::<gst::Element>().filter(|element| {
            element
                .factory()
                .is_some_and(|factory| factory.name() == WASAPI2_FACTORY)
        })
    }) else {
        return false;
    };
    if sink.find_property("device").is_none()
        || sink.find_property("continue-on-error").is_none()
        || !sink.property::<bool>("continue-on-error")
    {
        return false;
    }

    reapply_cached_volume(playbin, volume);
    if claim_warning_recovery(recovery_claimed) {
        // Setting the unchanged device is meaningful to wasapi2sink: it
        // reconnects only when its invalidation latch is set.
        let device = sink.property::<Option<String>>("device");
        sink.set_property("device", device.as_deref());
        warn!("Windows audio endpoint was invalidated; requested one bounded reconnection");
    }
    true
}

#[cfg(any(target_os = "windows", test))]
fn claim_warning_recovery(recovery_claimed: &Cell<bool>) -> bool {
    !recovery_claimed.replace(true)
}

#[cfg(any(target_os = "windows", test))]
fn reapply_cached_volume(playbin: &gst::Element, volume: f64) {
    playbin.set_property("volume", super::slider_to_pipeline(volume));
}

#[cfg(any(target_os = "windows", test))]
fn default_wasapi2_endpoint_id(properties: Option<&gst::StructureRef>) -> Option<String> {
    let properties = properties?;
    if properties.get::<&str>("device.api").ok()? != WASAPI2_API
        || !properties.get::<bool>("device.default").ok()?
    {
        return None;
    }

    properties
        .get::<&str>("device.actual-id")
        .ok()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(api: &str, is_default: bool, actual_id: &str) -> gst::Structure {
        gst::init().expect("initialize GStreamer");
        gst::Structure::builder("device-properties")
            .field("device.api", api)
            .field("device.default", is_default)
            .field("device.actual-id", actual_id)
            .build()
    }

    #[test]
    fn accepts_only_a_default_wasapi2_render_endpoint() {
        let valid = properties(WASAPI2_API, true, "{render-endpoint}");
        assert_eq!(
            default_wasapi2_endpoint_id(Some(valid.as_ref())).as_deref(),
            Some("{render-endpoint}")
        );

        let other_api = properties("pipewire", true, "node-1");
        assert!(default_wasapi2_endpoint_id(Some(other_api.as_ref())).is_none());

        let non_default = properties(WASAPI2_API, false, "{render-endpoint}");
        assert!(default_wasapi2_endpoint_id(Some(non_default.as_ref())).is_none());
    }

    #[test]
    fn rejects_missing_or_empty_default_endpoint_identity() {
        gst::init().expect("initialize GStreamer");
        let missing = gst::Structure::builder("device-properties")
            .field("device.api", WASAPI2_API)
            .field("device.default", true)
            .build();
        assert!(default_wasapi2_endpoint_id(Some(missing.as_ref())).is_none());

        let empty = properties(WASAPI2_API, true, " \t");
        assert!(default_wasapi2_endpoint_id(Some(empty.as_ref())).is_none());
        assert!(default_wasapi2_endpoint_id(None).is_none());
    }

    #[test]
    fn warning_recovery_is_single_flight_until_an_external_reset() {
        let claimed = Cell::new(false);

        assert!(claim_warning_recovery(&claimed));
        assert!(!claim_warning_recovery(&claimed));
        claimed.set(false);
        assert!(claim_warning_recovery(&claimed));
    }
}
