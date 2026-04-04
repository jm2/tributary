//! `SourceObject` — GObject wrapper for sidebar media sources.
//!
//! Supports three kinds of row:
//! * **Header** — non-selectable category label (`is_header = true`)
//! * **Discovered** — unauthenticated remote server (`server_url` set, `connected = false`)
//! * **Connected** — active backend (`connected = true`)
//! * **Local** — the local filesystem source (no `server_url`)

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::subclass::prelude::*;

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
        /// Whether this remote source has been authenticated and connected.
        pub connected: Cell<bool>,
        /// Whether an authentication attempt is in progress.
        pub connecting: Cell<bool>,
        /// DAAP logout URL for session cleanup on disconnect.
        pub logout_url: RefCell<String>,
        /// Whether this server requires a password to connect.
        /// `true` = password required (default), `false` = open/passwordless.
        pub requires_password: Cell<bool>,
        /// Whether this server was manually added by the user (persisted in
        /// `servers.json`). Manually-added servers are never auto-removed by
        /// discovery refresh and show a trash/delete button in the sidebar.
        pub manually_added: Cell<bool>,
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
        obj.imp().is_header.set(false);
        obj.imp().connected.set(false);
        obj.imp().requires_password.set(true); // assume locked until probed
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
    pub fn connected(&self) -> bool {
        self.imp().connected.get()
    }

    // ── Mutators (for transitioning Discovered → Connected) ─────────

    pub fn set_connected(&self, val: bool) {
        self.imp().connected.set(val);
    }

    pub fn set_icon_name(&self, name: &str) {
        self.imp().icon_name.replace(name.to_string());
    }

    pub fn connecting(&self) -> bool {
        self.imp().connecting.get()
    }

    pub fn set_connecting(&self, val: bool) {
        self.imp().connecting.set(val);
    }

    pub fn logout_url(&self) -> String {
        self.imp().logout_url.borrow().clone()
    }

    pub fn set_logout_url(&self, url: &str) {
        self.imp().logout_url.replace(url.to_string());
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

    /// Create a manually-added (unauthenticated) remote server row.
    ///
    /// Similar to `discovered()` but sets `manually_added = true` so the
    /// server is never auto-removed by discovery refresh and shows a
    /// trash/delete button in the sidebar.
    pub fn manual(name: &str, backend_type: &str, server_url: &str) -> Self {
        let obj = Self::discovered(name, backend_type, server_url);
        obj.imp().manually_added.set(true);
        obj
    }
}
