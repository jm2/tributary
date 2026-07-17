//! `SourceObject` — GObject wrapper for sidebar media sources.
//!
//! Supports the sidebar's distinct row kinds:
//! * **Header** — non-selectable category label (`is_header = true`)
//! * **Discovered** — unauthenticated remote server (`server_url` set, `connected = false`)
//! * **Connected** — active backend (`connected = true`)
//! * **Local** — the local filesystem source (no `server_url`)
//! * **Removable device** — logical source identity plus an owned native mount path

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use gtk::glib;
use gtk::subclass::prelude::*;

use crate::architecture::{AdvertisedHttpRoute, SourceId};

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct SourceObject {
        pub name: RefCell<String>,
        pub backend_type: RefCell<String>,
        pub icon_name: RefCell<String>,
        pub is_header: Cell<bool>,
        /// Base URL for remote servers (e.g. `https://music.example.com`).
        pub server_url: RefCell<String>,
        /// Stable logical source identity. This is independent from the
        /// endpoint and from the UI navigation/view key.
        pub source_id: RefCell<String>,
        /// Ephemeral address route supplied by network discovery. This is
        /// deliberately separate from the persisted URL and stable logical
        /// identity; a connection generation snapshots it when authentication
        /// begins.
        pub(super) advertised_route: RefCell<Option<AdvertisedHttpRoute>>,
        /// Logical identity kept separate from location for sources such as a
        /// removable filesystem remounted at a different path.
        pub source_key: RefCell<String>,
        /// Native mount path for a removable device. Kept as a `PathBuf` so
        /// non-UTF-8 paths are never corrupted by a lossy string conversion.
        pub device_mount_point: RefCell<Option<PathBuf>>,
        /// Whether this remote source has been authenticated and connected.
        pub connected: Cell<bool>,
        /// Whether an authentication attempt is in progress.
        pub connecting: Cell<bool>,
        /// Whether this server requires a password to connect.
        /// `true` = password required (default), `false` = open/passwordless.
        pub requires_password: Cell<bool>,
        /// Whether this server was manually added by the user (persisted in
        /// `servers.json`). Manually-added servers are never auto-removed by
        /// discovery refresh and show a trash/delete button in the sidebar.
        pub manually_added: Cell<bool>,
        /// Playlist UUID for playlist sidebar entries.
        pub playlist_id: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SourceObject {
        const NAME: &'static str = "TributarySourceObject";
        type Type = super::SourceObject;
    }

    impl ObjectImpl for SourceObject {}
}

glib::wrapper! {
    pub struct SourceObject(ObjectSubclass<imp::SourceObject>);
}

impl SourceObject {
    /// Create a non-selectable category header row.
    pub fn header(name: &str) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        obj.imp().is_header.set(true);
        obj
    }

    /// Create a local or static source row.
    pub fn source(name: &str, backend_type: &str, icon_name: &str) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        obj.imp().backend_type.replace(backend_type.to_string());
        obj.imp().icon_name.replace(icon_name.to_string());
        obj.imp().is_header.set(false);
        obj.imp().connected.set(true); // local sources are always "connected"
        if backend_type == "local" {
            obj.imp().source_id.replace(SourceId::local().to_string());
        } else if backend_type.starts_with("radio-") {
            obj.imp()
                .source_id
                .replace(SourceId::radio_browser().to_string());
        }
        obj
    }

    /// Create a discovered (unauthenticated) remote server row.
    pub fn discovered(name: &str, backend_type: &str, server_url: &str) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        obj.imp().backend_type.replace(backend_type.to_string());
        obj.imp()
            .icon_name
            .replace("network-server-symbolic".to_string());
        obj.imp().server_url.replace(server_url.to_string());
        if let Ok(parsed) = crate::http_security::parse_base_url(server_url) {
            if let Ok(source_id) = SourceId::remote(backend_type, &parsed) {
                obj.imp().source_id.replace(source_id.to_string());
            }
        }
        obj.imp().is_header.set(false);
        obj.imp().connected.set(false);
        // Assume open until probed. forked-daapd / OwnTone / iTunes shares
        // default to no password; defaulting `true` here caused a race where
        // a click before `probe_daap_password` finished would force-show the
        // auth dialog even for open shares. The connect path now retries via
        // the auth dialog if a passwordless connect comes back with
        // `AuthenticationFailed`, so a wrong guess for password-protected
        // shares self-corrects on the failure response.
        obj.imp().requires_password.set(false);
        obj
    }

    /// Create the non-selectable heading for removable-device rows.
    pub fn device_header(name: &str) -> Self {
        let obj = Self::header(name);
        obj.imp()
            .backend_type
            .replace("usb-device-header".to_string());
        obj
    }

    /// Create one mounted removable-device source.
    pub fn removable_device(name: &str, source_key: &str, mount_point: PathBuf) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        obj.imp().backend_type.replace("usb-device".to_string());
        obj.imp()
            .icon_name
            .replace("drive-removable-media-symbolic".to_string());
        obj.imp().source_key.replace(source_key.to_string());
        if let Ok(source_id) = SourceId::removable(source_key) {
            obj.imp().source_id.replace(source_id.to_string());
        }
        obj.imp().device_mount_point.replace(Some(mount_point));
        obj.imp().is_header.set(false);
        obj.imp().connected.set(true);
        obj.imp().requires_password.set(false);
        obj
    }

    // ── Getters ─────────────────────────────────────────────────────

    pub fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }
    pub fn backend_type(&self) -> String {
        self.imp().backend_type.borrow().clone()
    }
    pub fn icon_name(&self) -> String {
        self.imp().icon_name.borrow().clone()
    }
    pub fn is_header(&self) -> bool {
        self.imp().is_header.get()
    }
    pub fn server_url(&self) -> String {
        self.imp().server_url.borrow().clone()
    }
    pub fn source_id(&self) -> Option<SourceId> {
        self.imp().source_id.borrow().parse().ok()
    }
    pub(crate) fn advertised_route(&self) -> Option<AdvertisedHttpRoute> {
        self.imp().advertised_route.borrow().clone()
    }
    pub fn source_key(&self) -> String {
        self.imp().source_key.borrow().clone()
    }
    pub fn device_mount_point(&self) -> Option<PathBuf> {
        self.imp().device_mount_point.borrow().clone()
    }
    pub fn connected(&self) -> bool {
        self.imp().connected.get()
    }

    // ── Mutators (for transitioning Discovered → Connected) ─────────

    pub fn set_connected(&self, val: bool) {
        self.imp().connected.set(val);
    }

    pub fn set_name(&self, name: &str) {
        self.imp().name.replace(name.to_string());
    }

    pub fn set_server_url(&self, server_url: &str) {
        self.imp().server_url.replace(server_url.to_string());
    }

    pub fn set_source_id(&self, source_id: SourceId) {
        self.imp().source_id.replace(source_id.to_string());
    }

    pub fn set_icon_name(&self, name: &str) {
        self.imp().icon_name.replace(name.to_string());
    }

    pub(crate) fn set_advertised_route(&self, route: Option<AdvertisedHttpRoute>) {
        self.imp().advertised_route.replace(route);
    }

    pub fn connecting(&self) -> bool {
        self.imp().connecting.get()
    }

    pub fn set_connecting(&self, val: bool) {
        self.imp().connecting.set(val);
    }

    pub fn requires_password(&self) -> bool {
        self.imp().requires_password.get()
    }

    pub fn set_requires_password(&self, val: bool) {
        self.imp().requires_password.set(val);
    }

    pub fn manually_added(&self) -> bool {
        self.imp().manually_added.get()
    }

    pub fn set_manually_added(&self, val: bool) {
        self.imp().manually_added.set(val);
    }

    pub fn playlist_id(&self) -> String {
        self.imp().playlist_id.borrow().clone()
    }

    /// Create a playlist sidebar entry.
    pub fn playlist(name: &str, playlist_id: &str, is_smart: bool) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        let bt = if is_smart {
            "smart-playlist"
        } else {
            "playlist"
        };
        obj.imp().backend_type.replace(bt.to_string());
        let icon = if is_smart {
            "emblem-system-symbolic"
        } else {
            "view-list-symbolic"
        };
        obj.imp().icon_name.replace(icon.to_string());
        obj.imp().is_header.set(false);
        obj.imp().connected.set(true);
        obj.imp().playlist_id.replace(playlist_id.to_string());
        obj
    }

    /// Create a manually-added (unauthenticated) remote server row.
    ///
    /// Similar to `discovered()` but sets `manually_added = true` so the
    /// server is never auto-removed by discovery refresh and shows a
    /// trash/delete button in the sidebar.
    pub fn manual(name: &str, backend_type: &str, server_url: &str, source_id: SourceId) -> Self {
        let obj = Self::discovered(name, backend_type, server_url);
        obj.set_source_id(source_id);
        obj.imp().manually_added.set(true);
        obj
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use super::SourceObject;
    use crate::architecture::{AdvertisedHttpRoute, SourceId};

    #[test]
    fn discovered_route_is_ephemeral_and_does_not_change_source_identity() {
        let source =
            SourceObject::discovered("Living Room", "subsonic", "http://mini-2.local:4533");
        let origin = url::Url::parse("http://mini-2.local:4533").expect("origin");
        let route = AdvertisedHttpRoute::new(
            &origin,
            ["192.0.2.44:4533".parse::<SocketAddr>().expect("address")],
        )
        .expect("advertised route");

        assert_eq!(source.advertised_route(), None);
        source.set_advertised_route(Some(route.clone()));
        assert_eq!(source.advertised_route(), Some(route));
        assert_eq!(source.name(), "Living Room");
        assert_eq!(source.server_url(), "http://mini-2.local:4533");
        let expected_source_id = SourceId::remote("subsonic", &origin).expect("source ID");
        assert_eq!(source.source_id(), Some(expected_source_id));
        assert!(source.source_key().is_empty());
        assert!(!source.manually_added());
        assert!(!source.connected());

        source.set_advertised_route(None);
        assert_eq!(source.advertised_route(), None);
        assert_eq!(source.name(), "Living Room");
        assert_eq!(source.server_url(), "http://mini-2.local:4533");
        assert_eq!(source.source_id(), Some(expected_source_id));
    }

    #[test]
    fn removable_device_preserves_identity_and_mount_path() {
        let mount_point = PathBuf::from("/media/listener/MIXTAPE");
        let source = SourceObject::removable_device(
            "MIXTAPE",
            "device:uuid:01234567-89ab-cdef-0123-456789abcdef",
            mount_point.clone(),
        );

        assert_eq!(source.name(), "MIXTAPE");
        assert_eq!(source.backend_type(), "usb-device");
        assert_eq!(source.icon_name(), "drive-removable-media-symbolic");
        assert!(!source.is_header());
        assert!(source.connected());
        assert!(!source.connecting());
        assert!(!source.requires_password());
        assert_eq!(
            source.source_key(),
            "device:uuid:01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(source.device_mount_point(), Some(mount_point));
        assert_eq!(
            source.source_id(),
            Some(
                SourceId::removable("device:uuid:01234567-89ab-cdef-0123-456789abcdef")
                    .expect("source ID")
            )
        );
        assert!(source.server_url().is_empty());
    }

    #[test]
    fn persisted_manual_identity_overrides_endpoint_derived_identity() {
        let persisted = SourceId::random();
        let source =
            SourceObject::manual("Saved", "subsonic", "https://music.example.test", persisted);

        assert_eq!(source.source_id(), Some(persisted));
        assert!(source.manually_added());
    }

    #[test]
    fn device_header_is_explicitly_namespaced_and_non_selectable() {
        let header = SourceObject::device_header("Devices");

        assert_eq!(header.name(), "Devices");
        assert_eq!(header.backend_type(), "usb-device-header");
        assert!(header.is_header());
        assert!(header.source_key().is_empty());
        assert_eq!(header.device_mount_point(), None);
    }

    #[cfg(unix)]
    #[test]
    fn removable_device_preserves_a_non_utf8_mount_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mount_point = PathBuf::from(OsString::from_vec(
            b"/media/listener/non-utf8-\xff".to_vec(),
        ));
        let source = SourceObject::removable_device(
            "Non-UTF-8 device",
            "device:root:file:///media/listener/non-utf8",
            mount_point.clone(),
        );

        assert_eq!(source.device_mount_point(), Some(mount_point));
    }
}
