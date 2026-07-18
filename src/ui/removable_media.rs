//! Native removable-volume monitoring and sidebar reconciliation.
//!
//! `gio::VolumeMonitor` and every object it returns stay on GTK's main
//! thread. The monitor supplies cached mount metadata and lifecycle intent;
//! the source registry owns cancellable filesystem walks, tag parsing,
//! accepted catalogues, at-use file authority, and retirement.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::architecture::SourceId;
use crate::device::DeviceInfo;
use crate::source_lifecycle::SourceProvenance;

use super::objects::SourceObject;
use super::source_navigation::{SourceNavigation, SourceRequest};
use super::window_state::WindowState;

type SourcePlaybackInvalidator = Rc<dyn Fn(&str)>;

#[derive(Debug, Default, Eq, PartialEq)]
struct ReconciliationPlan {
    /// Sources whose cached scan/navigation/playback state is no longer valid.
    retired_keys: Vec<String>,
    /// Rows that disappeared completely from the new snapshot.
    removed_keys: Vec<String>,
    /// New, renamed, or relocated rows to insert or replace.
    upserts: Vec<DeviceInfo>,
    /// An active logical device whose path changed should remain selected.
    reactivate_key: Option<String>,
}

#[derive(Debug)]
struct PendingReactivation {
    source_key: String,
    fallback_request: SourceRequest,
}

fn pending_reactivation_key(
    pending: Option<PendingReactivation>,
    next: &BTreeMap<String, DeviceInfo>,
    navigation: &SourceNavigation,
) -> Option<String> {
    pending.and_then(|pending| {
        (next.contains_key(&pending.source_key) && navigation.is_current(&pending.fallback_request))
            .then_some(pending.source_key)
    })
}

fn inventory(devices: Vec<DeviceInfo>) -> BTreeMap<String, DeviceInfo> {
    devices
        .into_iter()
        .filter_map(|device| {
            removable_ui_key(&device.source_key).map(|_| (device.source_key.clone(), device))
        })
        .collect()
}

fn removable_source_id(source_key: &str) -> Option<SourceId> {
    SourceId::removable(source_key).ok()
}

fn removable_ui_key(source_key: &str) -> Option<String> {
    removable_source_id(source_key).map(|source_id| source_id.to_string())
}

fn planned_reconnections(
    pending: &HashSet<String>,
    next: &BTreeMap<String, DeviceInfo>,
    replaced: &[String],
) -> Vec<String> {
    let mut keys: Vec<_> = pending
        .iter()
        .filter(|key| next.contains_key(*key) && !replaced.contains(*key))
        .cloned()
        .collect();
    keys.sort();
    keys
}

fn device_keys_at_mount_point(
    devices: &BTreeMap<String, DeviceInfo>,
    mount_point: &std::path::Path,
) -> Vec<String> {
    devices
        .iter()
        .filter(|(_, device)| device.mount_point == mount_point)
        .map(|(key, _)| key.clone())
        .collect()
}

fn remove_devices_at_mount_point(
    devices: &mut BTreeMap<String, DeviceInfo>,
    mount_point: &std::path::Path,
) -> Vec<String> {
    let keys = device_keys_at_mount_point(devices, mount_point);
    for key in &keys {
        devices.remove(key);
    }
    keys
}

fn plan_reconciliation(
    current: &BTreeMap<String, DeviceInfo>,
    next: &BTreeMap<String, DeviceInfo>,
    active_source_key: &str,
) -> ReconciliationPlan {
    let mut plan = ReconciliationPlan::default();

    for (key, old) in current {
        match next.get(key) {
            None => {
                plan.retired_keys.push(key.clone());
                plan.removed_keys.push(key.clone());
            }
            Some(new) if old.mount_point != new.mount_point => {
                plan.retired_keys.push(key.clone());
                plan.upserts.push(new.clone());
                if removable_ui_key(key).as_deref() == Some(active_source_key) {
                    plan.reactivate_key = Some(key.clone());
                }
            }
            Some(new) if old.name != new.name => plan.upserts.push(new.clone()),
            Some(_) => {}
        }
    }

    for (key, device) in next {
        if !current.contains_key(key) {
            plan.upserts.push(device.clone());
        }
    }
    plan.upserts
        .sort_by(|left, right| left.source_key.cmp(&right.source_key));
    plan
}

struct RemovableMediaController {
    monitor: gio::VolumeMonitor,
    monitor_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    stopped: Cell<bool>,
    reconcile_scheduled: Cell<bool>,
    devices: RefCell<BTreeMap<String, DeviceInfo>>,
    pending_reconnect: RefCell<HashSet<String>>,
    pending_reactivation: RefCell<Option<PendingReactivation>>,
    devices_heading: String,
    device_fallback_name: String,
    sidebar_store: gio::ListStore,
    sidebar_selection: gtk::SingleSelection,
    source_tracks: Rc<RefCell<std::collections::HashMap<String, Vec<super::objects::TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    source_navigation: Rc<RefCell<SourceNavigation>>,
    source_registry: crate::source_registry::SourceRegistry,
    source_provenance: crate::source_registry::ProvenanceClaims,
    invalidate_source_playback: SourcePlaybackInvalidator,
}

impl RemovableMediaController {
    fn new(
        state: &WindowState,
        monitor: gio::VolumeMonitor,
        invalidate_source_playback: SourcePlaybackInvalidator,
    ) -> Rc<Self> {
        Rc::new(Self {
            monitor,
            monitor_handlers: RefCell::new(Vec::new()),
            stopped: Cell::new(false),
            reconcile_scheduled: Cell::new(false),
            devices: RefCell::new(BTreeMap::new()),
            pending_reconnect: RefCell::new(HashSet::new()),
            pending_reactivation: RefCell::new(None),
            devices_heading: rust_i18n::t!("sidebar.devices").into_owned(),
            device_fallback_name: rust_i18n::t!("sidebar.usb_device").into_owned(),
            sidebar_store: state.sidebar_store.clone(),
            sidebar_selection: state.sidebar_selection.clone(),
            source_tracks: state.source_tracks.clone(),
            active_source_key: state.active_source_key.clone(),
            source_navigation: state.source_navigation.clone(),
            source_registry: state.source_registry.clone(),
            source_provenance: state.remote_provenance.clone(),
            invalidate_source_playback,
        })
    }

    fn connect_monitor(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.monitor_handlers
            .borrow_mut()
            .push(self.monitor.connect_mount_added(move |_, _| {
                if let Some(controller) = weak.upgrade() {
                    controller.schedule_reconciliation();
                }
            }));

        let weak = Rc::downgrade(self);
        self.monitor_handlers
            .borrow_mut()
            .push(self.monitor.connect_mount_changed(move |_, _| {
                if let Some(controller) = weak.upgrade() {
                    controller.schedule_reconciliation();
                }
            }));

        let weak = Rc::downgrade(self);
        self.monitor_handlers
            .borrow_mut()
            .push(self.monitor.connect_mount_removed(move |_, mount| {
                if let Some(controller) = weak.upgrade() {
                    // Retire the exact old namespace synchronously. Deferring
                    // this solely to an idle snapshot lets a fast same-path
                    // reattach make old and new snapshots look identical.
                    if let Some(path) = mount.root().path() {
                        controller.mount_removed(&path);
                    }
                    controller.schedule_reconciliation();
                }
            }));

        // Retire scans and playback before the namespace disappears. This
        // signal is optional and an unmount may still fail, so the row and
        // inventory remain until mount-removed confirms the transition.
        let weak = Rc::downgrade(self);
        self.monitor_handlers
            .borrow_mut()
            .push(self.monitor.connect_mount_pre_unmount(move |_, mount| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                if let Some(path) = mount.root().path() {
                    controller.pre_unmount(&path);
                }
            }));
    }

    fn schedule_reconciliation(self: &Rc<Self>) {
        if self.stopped.get() || self.reconcile_scheduled.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(controller) = weak.upgrade() {
                controller.reconcile_scheduled.set(false);
                controller.reconcile();
            }
        });
    }

    fn reconcile(&self) {
        if self.stopped.get() {
            return;
        }

        let next = inventory(crate::device::usb::mounted_devices(&self.monitor));
        let current = self.devices.borrow().clone();
        let active = self.active_source_key.borrow().clone();
        let plan = plan_reconciliation(&current, &next, &active);
        let reconnect_keys =
            planned_reconnections(&self.pending_reconnect.borrow(), &next, &plan.retired_keys);
        let pending_reactivation = self.pending_reactivation.borrow_mut().take();

        let _ = self.retire_sources(&plan.retired_keys, false);

        for key in &plan.removed_keys {
            self.release_source_claim(key);
            self.remove_device_row(key);
        }
        for device in &plan.upserts {
            let needs_connection = !current.contains_key(&device.source_key)
                || plan.retired_keys.contains(&device.source_key);
            if needs_connection {
                self.connect_source(device);
            }
            self.upsert_device_row(device);
        }
        for key in reconnect_keys {
            if let Some(device) = next.get(&key) {
                self.connect_source(device);
            }
        }
        self.sync_device_header(!next.is_empty());
        *self.devices.borrow_mut() = next;

        let pending_key = pending_reactivation_key(
            pending_reactivation,
            &self.devices.borrow(),
            &self.source_navigation.borrow(),
        );
        if let Some(key) = plan.reactivate_key.or(pending_key) {
            self.select_device_source(&key);
        }
    }

    fn pre_unmount(self: &Rc<Self>, mount_point: &std::path::Path) {
        if self.stopped.get() {
            return;
        }
        let keys = device_keys_at_mount_point(&self.devices.borrow(), mount_point);
        self.pending_reconnect
            .borrow_mut()
            .extend(keys.iter().cloned());
        if let Some(pending) = self.retire_sources(&keys, false) {
            *self.pending_reactivation.borrow_mut() = Some(pending);
        }
        // A pre-unmount notification is advisory and the unmount may fail.
        // Only a fresh mounted-device snapshot may create the replacement
        // epoch; a confirmed removal clears the pending reconnect first.
        self.schedule_reconciliation();
    }

    fn mount_removed(&self, mount_point: &std::path::Path) {
        if self.stopped.get() {
            return;
        }

        let keys = remove_devices_at_mount_point(&mut self.devices.borrow_mut(), mount_point);
        if let Some(pending) = self.retire_sources(&keys, true) {
            *self.pending_reactivation.borrow_mut() = Some(pending);
        }
        for key in &keys {
            self.remove_device_row(key);
        }
        self.sync_device_header(!self.devices.borrow().is_empty());
    }

    fn retire_sources(
        &self,
        source_keys: &[String],
        release_claims: bool,
    ) -> Option<PendingReactivation> {
        let mut active_logical_key = None;
        for key in source_keys {
            if self.retire_source(key) {
                active_logical_key = Some(key.clone());
            }
            if release_claims {
                self.release_source_claim(key);
            }
        }
        if let Some(source_key) = active_logical_key {
            let fallback_request = self.select_local_source();
            Some(PendingReactivation {
                source_key,
                fallback_request,
            })
        } else {
            None
        }
    }

    fn retire_source(&self, logical_key: &str) -> bool {
        let Some(source_id) = removable_source_id(logical_key) else {
            return false;
        };
        let source_key = source_id.to_string();

        // Disconnect synchronously cancels the exact scan, clears accepted
        // snapshots, and revokes media leases before GTK cache/playback state
        // can observe the retired namespace.
        let _ = self.source_registry.disconnect(source_id);
        self.source_navigation
            .borrow_mut()
            .invalidate_key(&source_key);
        self.source_tracks.borrow_mut().remove(&source_key);
        (self.invalidate_source_playback)(&source_key);
        *self.active_source_key.borrow() == source_key
    }

    fn connect_source(&self, device: &DeviceInfo) {
        let Some(source_id) = removable_source_id(&device.source_key) else {
            return;
        };
        if !self.source_provenance.ensure(
            &self.source_registry,
            source_id,
            SourceProvenance::Removable,
            device.source_key.clone(),
        ) {
            return;
        }

        if self
            .source_registry
            .connect_removable(source_id, device.mount_point.clone(), |_| {})
            .is_some()
        {
            self.pending_reconnect
                .borrow_mut()
                .remove(&device.source_key);
        } else {
            self.pending_reconnect
                .borrow_mut()
                .insert(device.source_key.clone());
        }
    }

    fn release_source_claim(&self, logical_key: &str) {
        self.pending_reconnect.borrow_mut().remove(logical_key);
        let Some(source_id) = removable_source_id(logical_key) else {
            return;
        };
        let _ = self.source_provenance.release(
            &self.source_registry,
            source_id,
            SourceProvenance::Removable,
            logical_key,
        );
    }

    fn select_local_source(&self) -> SourceRequest {
        if let Some(position) =
            self.find_row(|source| !source.is_header() && source.backend_type() == "local")
        {
            self.sidebar_selection.set_selected(position);
        }

        // Selection signals are synchronous, but preserve a safe navigation
        // identity even if the static Local row is absent or was already
        // selected while another component held inconsistent active state.
        let request = {
            let mut navigation = self.source_navigation.borrow_mut();
            if navigation.is_key("local") {
                navigation
                    .latest_request("local")
                    .unwrap_or_else(|| navigation.select("local"))
            } else {
                navigation.select("local")
            }
        };
        *self.active_source_key.borrow_mut() = "local".to_string();
        request
    }

    fn select_device_source(&self, source_key: &str) {
        if let Some(position) = self.find_device_row(source_key) {
            self.sidebar_selection.set_selected(position);
        }
    }

    fn upsert_device_row(&self, device: &DeviceInfo) {
        let name = if device.name.trim().is_empty() {
            &self.device_fallback_name
        } else {
            &device.name
        };
        let row =
            SourceObject::removable_device(name, &device.source_key, device.mount_point.clone());
        if let Some(position) = self.find_device_row(&device.source_key) {
            // Replace in one model notification. A remove followed by an
            // insert briefly selects a neighbouring source when this is the
            // active row, which can start unrelated source work during a
            // harmless volume-label update.
            self.sidebar_store.splice(position, 1, &[row]);
            return;
        }

        self.sync_device_header(true);
        let position = self.device_insert_position(&device.source_key);
        self.sidebar_store.insert(position, &row);
    }

    fn remove_device_row(&self, source_key: &str) {
        if let Some(position) = self.find_device_row(source_key) {
            self.sidebar_store.remove(position);
        }
    }

    fn sync_device_header(&self, needed: bool) {
        let header = self.find_row(|source| source.backend_type() == "usb-device-header");
        match (needed, header) {
            (true, None) => self
                .sidebar_store
                .append(&SourceObject::device_header(&self.devices_heading)),
            (false, Some(position)) => self.sidebar_store.remove(position),
            _ => {}
        }
    }

    fn device_insert_position(&self, source_key: &str) -> u32 {
        let Some(header_position) =
            self.find_row(|source| source.backend_type() == "usb-device-header")
        else {
            return self.sidebar_store.n_items();
        };

        // Keep the section contiguous even when another source category was
        // appended after Devices. Search only between this header and the
        // first non-device row, inserting before the first larger logical key.
        let mut position = header_position + 1;
        while position < self.sidebar_store.n_items() {
            let Some(source) = self
                .sidebar_store
                .item(position)
                .and_downcast::<SourceObject>()
            else {
                break;
            };
            if source.backend_type() != "usb-device" || source.source_key().as_str() > source_key {
                break;
            }
            position += 1;
        }
        position
    }

    fn find_device_row(&self, source_key: &str) -> Option<u32> {
        self.find_row(|source| {
            source.backend_type() == "usb-device" && source.source_key() == source_key
        })
    }

    fn find_row(&self, predicate: impl Fn(&SourceObject) -> bool) -> Option<u32> {
        (0..self.sidebar_store.n_items()).find(|position| {
            self.sidebar_store
                .item(*position)
                .and_downcast_ref::<SourceObject>()
                .is_some_and(&predicate)
        })
    }

    fn shutdown(&self) {
        if self.stopped.replace(true) {
            return;
        }
        // Stop accepting mount reconciliation before invalidating the
        // controller-owned navigation and cache state below.
        let keys: Vec<_> = self.devices.borrow().keys().cloned().collect();
        let mut navigation = self.source_navigation.borrow_mut();
        for key in &keys {
            if let Some(key) = removable_ui_key(key) {
                navigation.invalidate_key(&key);
            }
        }
        drop(navigation);
        self.pending_reconnect.borrow_mut().clear();
        let _ = self.pending_reactivation.borrow_mut().take();

        for handler in self.monitor_handlers.borrow_mut().drain(..) {
            self.monitor.disconnect(handler);
        }
    }
}

/// Monitor native mounted volumes and keep the removable-device sidebar
/// section synchronized for this window's lifetime.
pub(super) fn setup_removable_media(
    state: &WindowState,
    invalidate_source_playback: SourcePlaybackInvalidator,
) {
    let controller =
        RemovableMediaController::new(state, gio::VolumeMonitor::get(), invalidate_source_playback);
    controller.connect_monitor();
    controller.reconcile();

    // The global VolumeMonitor outlives the window. Its callbacks hold only a
    // Weak controller; this strong destroy closure defines the controller's
    // lifetime and disconnects every global signal deterministically.
    let controller_for_destroy = Rc::clone(&controller);
    state.window.connect_destroy(move |_| {
        controller_for_destroy.shutdown();
    });
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::path::PathBuf;

    use crate::device::DeviceInfo;

    use super::{
        device_keys_at_mount_point, inventory, pending_reactivation_key, plan_reconciliation,
        planned_reconnections, removable_ui_key, remove_devices_at_mount_point,
        PendingReactivation, ReconciliationPlan, SourceNavigation,
    };

    fn device(key: &str, name: &str, path: &str) -> DeviceInfo {
        DeviceInfo {
            source_key: key.to_string(),
            name: name.to_string(),
            mount_point: PathBuf::from(path),
        }
    }

    #[test]
    fn identical_snapshots_are_idempotent() {
        let current = inventory(vec![device("device:a", "A", "/media/a")]);

        assert_eq!(
            plan_reconciliation(&current, &current, "local"),
            ReconciliationPlan::default()
        );
    }

    #[test]
    fn removable_navigation_uses_source_identity_not_the_logical_gio_key() {
        let logical_key = "device:opaque-id";
        let ui_key = removable_ui_key(logical_key).expect("removable UI key");

        assert_ne!(ui_key, logical_key);
        assert_eq!(
            ui_key,
            crate::architecture::SourceId::removable(logical_key)
                .expect("source identity")
                .to_string()
        );

        let devices = inventory(vec![
            device("", "Missing identity", "/media/missing"),
            device(logical_key, "Player", "/media/player"),
        ]);
        assert_eq!(
            devices.keys().cloned().collect::<Vec<_>>(),
            vec![logical_key.to_string()]
        );
    }

    #[test]
    fn add_rename_and_remove_are_planned_deterministically() {
        let current = inventory(vec![
            device("device:b", "Old B", "/media/b"),
            device("device:gone", "Gone", "/media/gone"),
        ]);
        let next = inventory(vec![
            device("device:a", "A", "/media/a"),
            device("device:b", "New B", "/media/b"),
        ]);

        assert_eq!(
            plan_reconciliation(&current, &next, "local"),
            ReconciliationPlan {
                retired_keys: vec!["device:gone".to_string()],
                removed_keys: vec!["device:gone".to_string()],
                upserts: vec![
                    device("device:a", "A", "/media/a"),
                    device("device:b", "New B", "/media/b"),
                ],
                reactivate_key: None,
            }
        );
    }

    #[test]
    fn active_relocation_retires_old_state_and_reselects_the_same_identity() {
        let current = inventory(vec![device("device:a", "A", "/media/old")]);
        let next = inventory(vec![device("device:a", "A", "/media/new")]);
        let active = removable_ui_key("device:a").expect("active UI key");

        assert_eq!(
            plan_reconciliation(&current, &next, &active),
            ReconciliationPlan {
                retired_keys: vec!["device:a".to_string()],
                removed_keys: Vec::new(),
                upserts: vec![device("device:a", "A", "/media/new")],
                reactivate_key: Some("device:a".to_string()),
            }
        );
    }

    #[test]
    fn active_removal_falls_back_without_reactivating() {
        let current = inventory(vec![device("device:a", "A", "/media/a")]);
        let next = BTreeMap::new();
        let active = removable_ui_key("device:a").expect("active UI key");

        assert_eq!(
            plan_reconciliation(&current, &next, &active),
            ReconciliationPlan {
                retired_keys: vec!["device:a".to_string()],
                removed_keys: vec!["device:a".to_string()],
                upserts: Vec::new(),
                reactivate_key: None,
            }
        );
    }

    #[test]
    fn confirmed_remove_then_same_path_reattach_still_requires_a_fresh_upsert() {
        let device = device("device:a", "A", "/media/a");
        let mut current = inventory(vec![device.clone()]);

        let retired = remove_devices_at_mount_point(&mut current, std::path::Path::new("/media/a"));
        let reattached = inventory(vec![device.clone()]);

        assert_eq!(retired, vec!["device:a"]);
        assert!(current.is_empty());
        assert_eq!(
            plan_reconciliation(&current, &reattached, "local"),
            ReconciliationPlan {
                upserts: vec![device],
                ..ReconciliationPlan::default()
            }
        );
    }

    #[test]
    fn pre_unmount_lookup_retires_state_without_removing_a_still_mounted_device() {
        let current = inventory(vec![device("device:a", "A", "/media/a")]);

        assert_eq!(
            device_keys_at_mount_point(&current, std::path::Path::new("/media/a")),
            vec!["device:a"]
        );
        assert!(current.contains_key("device:a"));
    }

    #[test]
    fn pre_unmount_reconnects_only_from_a_fresh_matching_inventory() {
        let pending = HashSet::from(["device:a".to_string(), "device:gone".to_string()]);
        let next = inventory(vec![device("device:a", "A", "/media/a")]);

        assert_eq!(
            planned_reconnections(&pending, &next, &[]),
            vec!["device:a".to_string()]
        );
        assert!(planned_reconnections(&pending, &next, &["device:a".to_string()]).is_empty());
        assert!(planned_reconnections(&pending, &BTreeMap::new(), &[]).is_empty());
    }

    #[test]
    fn immediate_same_identity_reattach_restores_only_the_untouched_fallback() {
        let mut navigation = SourceNavigation::new("local");
        let device_ui_key = removable_ui_key("device:a").expect("device UI key");
        navigation.select(device_ui_key.clone());
        navigation.invalidate_key(&device_ui_key);
        let fallback_request = navigation.select("local");
        let pending = PendingReactivation {
            source_key: "device:a".to_string(),
            fallback_request: fallback_request.clone(),
        };
        let reattached = inventory(vec![device("device:a", "A", "/media/new")]);

        assert_eq!(
            pending_reactivation_key(Some(pending), &reattached, &navigation),
            Some("device:a".to_string())
        );

        let pending = PendingReactivation {
            source_key: "device:a".to_string(),
            fallback_request,
        };
        navigation.select("radio-topvote");
        assert_eq!(
            pending_reactivation_key(Some(pending), &reattached, &navigation),
            None
        );
    }
}
