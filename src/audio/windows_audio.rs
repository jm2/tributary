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
        let sink = configured_wasapi2_sink()?;
        let monitor = default_audio_monitor()?;
        let monitor_watch =
            watch_default_audio_endpoint(&monitor, &sink, playbin, volume, recovery_claimed)?;

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
fn configured_wasapi2_sink() -> Option<gst::Element> {
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
    Some(sink)
}

#[cfg(any(target_os = "windows", test))]
fn default_audio_monitor() -> Option<gst::DeviceMonitor> {
    let monitor = gst::DeviceMonitor::new();
    if monitor.add_filter(Some("Audio/Sink"), None).is_none() {
        warn!(
            "No Windows audio device provider available; system output changes use GStreamer's fallback behavior"
        );
        return None;
    }
    Some(monitor)
}

#[cfg(any(target_os = "windows", test))]
fn watch_default_audio_endpoint(
    monitor: &gst::DeviceMonitor,
    sink: &gst::Element,
    playbin: &gst::Element,
    volume: Rc<Cell<f64>>,
    recovery_claimed: Rc<Cell<bool>>,
) -> Option<gst::bus::BusWatchGuard> {
    let sink = sink.clone();
    let playbin = playbin.downgrade();
    let watch = monitor.bus().add_watch_local(move |_bus, message| {
        if let Some(endpoint_id) = default_wasapi2_endpoint_from_message(message) {
            recovery_claimed.set(false);
            sink.set_property("device", endpoint_id.as_str());
            if let Some(playbin) = playbin.upgrade() {
                reapply_cached_volume(&playbin, volume.get());
            }
            info!("Windows system audio output changed");
        }
        glib::ControlFlow::Continue
    });

    match watch {
        Ok(watch) => Some(watch),
        Err(error) => {
            warn!(
                error = %error,
                "Could not watch Windows audio devices; system output changes use GStreamer's fallback behavior"
            );
            None
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn default_wasapi2_endpoint_from_message(message: &gst::MessageRef) -> Option<String> {
    let device = match message.view() {
        gst::MessageView::DeviceAdded(added) => Some(added.device()),
        // GStreamer defines `device()` as the new immutable snapshot;
        // the generated `device_changed_()` accessor is the old one.
        gst::MessageView::DeviceChanged(changed) => Some(changed.device()),
        _ => None,
    }?;
    default_wasapi2_endpoint_id(device.properties().as_deref())
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
    let gst::MessageView::Warning(warning) = message.view() else {
        return false;
    };
    if !is_recoverable_wasapi2_warning_code(&warning.error()) {
        return false;
    }

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
fn is_recoverable_wasapi2_warning_code(error: &glib::Error) -> bool {
    error.matches(gst::ResourceError::OpenReadWrite) || error.matches(gst::ResourceError::Write)
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
    use gst::subclass::prelude::*;

    mod test_device {
        use super::*;

        #[derive(Default)]
        pub struct TestDevice;

        #[glib::object_subclass]
        impl ObjectSubclass for TestDevice {
            const NAME: &'static str = "TributaryWindowsAudioTestDevice";
            type Type = super::TestDevice;
            type ParentType = gst::Device;
        }

        impl ObjectImpl for TestDevice {}
        impl GstObjectImpl for TestDevice {}
        impl DeviceImpl for TestDevice {}
    }

    glib::wrapper! {
        pub struct TestDevice(ObjectSubclass<test_device::TestDevice>)
            @extends gst::Device, gst::Object;
    }

    fn properties(api: &str, is_default: bool, actual_id: &str) -> gst::Structure {
        gst::init().expect("initialize GStreamer");
        gst::Structure::builder("device-properties")
            .field("device.api", api)
            .field("device.default", is_default)
            .field("device.actual-id", actual_id)
            .build()
    }

    fn device(properties: gst::Structure) -> gst::Device {
        glib::Object::builder::<TestDevice>()
            .property("display-name", "Test output")
            .property("device-class", "Audio/Sink")
            .property("caps", gst::Caps::new_any())
            .property("properties", properties)
            .build()
            .upcast()
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
    fn device_changed_selects_the_new_snapshot_not_the_replaced_one() {
        gst::init().expect("initialize GStreamer");
        let replacement = device(properties(WASAPI2_API, true, "{new-default}"));
        let replaced = device(properties(WASAPI2_API, false, "{old-default}"));
        let message = gst::message::DeviceChanged::new(&replacement, &replaced);

        assert_eq!(
            default_wasapi2_endpoint_from_message(message.as_ref()).as_deref(),
            Some("{new-default}")
        );
    }

    #[test]
    fn warning_recovery_is_single_flight_until_an_external_reset() {
        let claimed = Cell::new(false);

        assert!(claim_warning_recovery(&claimed));
        assert!(!claim_warning_recovery(&claimed));
        claimed.set(false);
        assert!(claim_warning_recovery(&claimed));
    }

    #[test]
    fn only_wasapi2_output_device_warning_codes_are_recoverable() {
        let open = glib::Error::new(gst::ResourceError::OpenReadWrite, "open");
        let write = glib::Error::new(gst::ResourceError::Write, "write");
        let unrelated_resource = glib::Error::new(gst::ResourceError::Read, "read");
        let unrelated_domain = glib::Error::new(gst::CoreError::Failed, "core");

        assert!(is_recoverable_wasapi2_warning_code(&open));
        assert!(is_recoverable_wasapi2_warning_code(&write));
        assert!(!is_recoverable_wasapi2_warning_code(&unrelated_resource));
        assert!(!is_recoverable_wasapi2_warning_code(&unrelated_domain));
    }
}
