//! Mounted removable-device discovery through GIO.
//!
//! `GVolumeMonitor` already applies the platform's definition of mounts that
//! are interesting to a desktop user. This module projects its cached mount
//! metadata into plain [`DeviceInfo`] values; it performs no filesystem I/O.

use std::path::PathBuf;

use gtk::gio;
use gtk::prelude::*;
use tracing::{debug, info};

use super::DeviceInfo;

/// Cached GIO metadata used by the pure filtering and identity policy.
///
/// Keeping this separate from `gio::Mount` makes the platform policy fully
/// deterministic in tests without trying to fabricate a GIO mount backend.
#[derive(Clone, Debug)]
struct MountFacts {
    name: String,
    mount_point: Option<PathBuf>,
    root_uri: String,
    root_is_native: bool,
    shadowed: bool,
    mount_uuid: Option<String>,
    volume_uuid: Option<String>,
    volume_unix_device: Option<String>,
    volume_class: Option<String>,
    capabilities: MountCapabilities,
}

#[derive(Clone, Copy, Debug, Default)]
struct MountCapabilities {
    drive_removable: bool,
    can_eject: bool,
    can_unmount: bool,
}

/// Return the currently mounted, browseable devices known to `monitor`.
///
/// `GVolumeMonitor` is not thread-default-context aware. Call this on the GTK
/// main thread, where the UI also owns the monitor's mount-added, changed, and
/// removed signal handlers. All methods used here read GIO's cached object
/// metadata; this function never probes, canonicalizes, or enumerates a path.
///
/// GIO backends do not expose one uniformly reliable "USB" bit. We exclude
/// shadowed mounts, roots without native-path access, and mounts a backend
/// explicitly classifies as network or loop. We then retain a user-visible
/// native-path mount when the backend says its drive is removable, it can be
/// ejected or unmounted, or its volume class is `device`. In particular,
/// `can_unmount` preserves useful mounts on minimal systems where the backend
/// has no corresponding `GDrive` or `GVolume`; because that flag is broad and
/// class metadata is optional, it can also admit a non-removable or natively
/// mounted network filesystem.
pub fn mounted_devices(monitor: &gio::VolumeMonitor) -> Vec<DeviceInfo> {
    let devices = devices_from_facts(monitor.mounts().iter().map(mount_facts));

    if devices.is_empty() {
        debug!("No mounted music devices detected");
    } else {
        info!(count = devices.len(), "Mounted music devices detected");
    }

    devices
}

fn mount_facts(mount: &gio::Mount) -> MountFacts {
    let root = mount.root();
    let volume = mount.volume();
    let drive = mount
        .drive()
        .or_else(|| volume.as_ref().and_then(|volume| volume.drive()));

    MountFacts {
        name: mount.name().to_string(),
        mount_point: root.path(),
        root_uri: root.uri().to_string(),
        root_is_native: root.is_native(),
        shadowed: mount.is_shadowed(),
        mount_uuid: mount.uuid().map(|uuid| uuid.to_string()),
        volume_uuid: volume
            .as_ref()
            .and_then(|volume| volume.uuid())
            .map(|uuid| uuid.to_string()),
        volume_unix_device: volume
            .as_ref()
            .and_then(|volume| volume.identifier(gio::VOLUME_IDENTIFIER_KIND_UNIX_DEVICE))
            .map(|identifier| identifier.to_string()),
        volume_class: volume
            .as_ref()
            .and_then(|volume| volume.identifier(gio::VOLUME_IDENTIFIER_KIND_CLASS))
            .map(|class| class.to_string()),
        capabilities: MountCapabilities {
            drive_removable: drive.as_ref().is_some_and(|drive| drive.is_removable()),
            can_eject: mount.can_eject()
                || volume.as_ref().is_some_and(|volume| volume.can_eject())
                || drive.as_ref().is_some_and(|drive| drive.can_eject()),
            can_unmount: mount.can_unmount(),
        },
    }
}

fn devices_from_facts(facts: impl IntoIterator<Item = MountFacts>) -> Vec<DeviceInfo> {
    let mut devices: Vec<_> = facts.into_iter().filter_map(device_from_facts).collect();

    // A filesystem UUID or device identifier may be exposed through more than
    // one mount alias. Sorting by identity and then path makes the retained
    // alias deterministic; the lexically first mount wins.
    devices.sort_unstable_by(|left, right| {
        left.source_key
            .cmp(&right.source_key)
            .then_with(|| left.mount_point.cmp(&right.mount_point))
            .then_with(|| left.name.cmp(&right.name))
    });
    devices.dedup_by(|left, right| left.source_key == right.source_key);
    devices
}

fn device_from_facts(facts: MountFacts) -> Option<DeviceInfo> {
    if facts.shadowed || !facts.root_is_native {
        return None;
    }
    let mount_point = facts.mount_point?;

    let volume_class = facts.volume_class.as_deref();
    if volume_class.is_some_and(|class| {
        class.eq_ignore_ascii_case("network") || class.eq_ignore_ascii_case("loop")
    }) {
        return None;
    }

    let class_is_device = volume_class.is_some_and(|class| class.eq_ignore_ascii_case("device"));
    if !(facts.capabilities.drive_removable
        || facts.capabilities.can_eject
        || class_is_device
        || facts.capabilities.can_unmount)
    {
        return None;
    }

    let source_key = nonempty_identity("usb:uuid:", facts.mount_uuid)
        .or_else(|| nonempty_identity("usb:uuid:", facts.volume_uuid))
        .or_else(|| nonempty_identity("usb:unix-device:", facts.volume_unix_device))
        .or_else(|| nonempty_identity("usb:root-uri:", Some(facts.root_uri)))?;

    Some(DeviceInfo {
        source_key,
        name: facts.name,
        mount_point,
    })
}

/// Prefix an opaque backend identifier without normalizing its value.
fn nonempty_identity(prefix: &str, value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(|value| {
        let mut identity = String::with_capacity(prefix.len() + value.len());
        identity.push_str(prefix);
        identity.push_str(&value);
        identity
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str, path: &str) -> MountFacts {
        MountFacts {
            name: name.to_string(),
            mount_point: Some(PathBuf::from(path)),
            root_uri: format!("file://{path}"),
            root_is_native: true,
            shadowed: false,
            mount_uuid: None,
            volume_uuid: None,
            volume_unix_device: None,
            volume_class: None,
            capabilities: MountCapabilities {
                can_unmount: true,
                ..MountCapabilities::default()
            },
        }
    }

    #[test]
    fn shadowed_pathless_and_non_native_mounts_are_rejected() {
        let mut shadowed = facts("shadowed", "/media/shadowed");
        shadowed.shadowed = true;
        let mut pathless = facts("pathless", "/media/pathless");
        pathless.mount_point = None;
        let mut non_native = facts("non-native", "/media/fuse-view");
        non_native.root_is_native = false;

        assert!(devices_from_facts([shadowed, pathless, non_native]).is_empty());
    }

    #[test]
    fn network_and_loop_classes_override_other_eligibility() {
        let mut network = facts("network", "/mnt/network");
        network.volume_class = Some("network".to_string());
        network.capabilities.drive_removable = true;
        network.capabilities.can_eject = true;
        let mut loop_mount = facts("loop", "/mnt/loop");
        loop_mount.volume_class = Some("LOOP".to_string());

        assert!(devices_from_facts([network, loop_mount]).is_empty());
    }

    #[test]
    fn internal_fixed_mount_without_device_capabilities_is_rejected() {
        let mut internal = facts("Internal", "/");
        internal.volume_class = Some("drive".to_string());
        internal.capabilities.can_unmount = false;

        assert!(device_from_facts(internal).is_none());
    }

    #[test]
    fn each_supported_eligibility_signal_admits_a_native_path_mount() {
        let mut removable = facts("removable", "/media/removable");
        removable.capabilities.can_unmount = false;
        removable.capabilities.drive_removable = true;

        let mut ejectable = facts("ejectable", "/media/ejectable");
        ejectable.capabilities.can_unmount = false;
        ejectable.capabilities.can_eject = true;

        let mut device_class = facts("device-class", "/media/device-class");
        device_class.capabilities.can_unmount = false;
        device_class.volume_class = Some("DEVICE".to_string());

        let unmountable = facts("unmountable", "/media/unmountable");

        assert_eq!(
            devices_from_facts([removable, ejectable, device_class, unmountable]).len(),
            4
        );
    }

    #[test]
    fn identity_prefers_mount_then_volume_uuid_then_unix_device_then_uri() {
        let mut all = facts("all", "/media/all");
        all.mount_uuid = Some("mount-id".to_string());
        all.volume_uuid = Some("volume-id".to_string());
        all.volume_unix_device = Some("/dev/example1".to_string());
        assert_eq!(
            device_from_facts(all).unwrap().source_key,
            "usb:uuid:mount-id"
        );

        let mut volume = facts("volume", "/media/volume");
        volume.mount_uuid = Some(String::new());
        volume.volume_uuid = Some("volume-id".to_string());
        volume.volume_unix_device = Some("/dev/example2".to_string());
        assert_eq!(
            device_from_facts(volume).unwrap().source_key,
            "usb:uuid:volume-id"
        );

        let mut unix_device = facts("unix", "/media/unix");
        unix_device.volume_unix_device = Some("/dev/example3".to_string());
        assert_eq!(
            device_from_facts(unix_device).unwrap().source_key,
            "usb:unix-device:/dev/example3"
        );

        let uri = facts("uri", "/media/uri");
        assert_eq!(
            device_from_facts(uri).unwrap().source_key,
            "usb:root-uri:file:///media/uri"
        );
    }

    #[test]
    fn opaque_identifiers_are_preserved_verbatim() {
        let mut opaque = facts("opaque", "/media/opaque");
        opaque.mount_uuid = Some("AbC-42 trailing ".to_string());

        assert_eq!(
            device_from_facts(opaque).unwrap().source_key,
            "usb:uuid:AbC-42 trailing "
        );
    }

    #[test]
    fn aliases_with_the_same_identity_keep_the_lexically_first_path() {
        let mut later = facts("later", "/media/z-alias");
        later.mount_uuid = Some("shared".to_string());
        let mut first = facts("first", "/media/a-alias");
        first.mount_uuid = Some("shared".to_string());

        let devices = devices_from_facts([later, first]);

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "first");
        assert_eq!(devices[0].mount_point, PathBuf::from("/media/a-alias"));
    }

    #[test]
    fn distinct_root_fallbacks_remain_distinct() {
        let first = facts("first", "/media/first");
        let second = facts("second", "/media/second");

        let devices = devices_from_facts([second, first]);

        assert_eq!(devices.len(), 2);
        assert_ne!(devices[0].source_key, devices[1].source_key);
    }

    #[test]
    fn output_order_is_deterministic_by_identity_then_path() {
        let mut beta = facts("beta", "/media/beta");
        beta.mount_uuid = Some("b".to_string());
        let mut alpha_later = facts("alpha-later", "/media/z-alpha");
        alpha_later.mount_uuid = Some("a".to_string());
        let mut alpha_first = facts("alpha-first", "/media/a-alpha");
        alpha_first.mount_uuid = Some("a".to_string());

        let forward = devices_from_facts([beta.clone(), alpha_later.clone(), alpha_first.clone()]);
        let reverse = devices_from_facts([alpha_first, alpha_later, beta]);

        assert_eq!(forward, reverse);
        assert_eq!(
            forward
                .iter()
                .map(|device| device.source_key.as_str())
                .collect::<Vec<_>>(),
            vec!["usb:uuid:a", "usb:uuid:b"]
        );
        assert_eq!(forward[0].mount_point, PathBuf::from("/media/a-alpha"));
    }
}
