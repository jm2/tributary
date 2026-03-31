//! `SourceObject` — GObject wrapper for sidebar media sources.

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
    pub fn header(name: &str) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        obj.imp().is_header.set(true);
        obj
    }

    pub fn source(name: &str, backend_type: &str, icon_name: &str) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().name.replace(name.to_string());
        obj.imp().backend_type.replace(backend_type.to_string());
        obj.imp().icon_name.replace(icon_name.to_string());
        obj.imp().is_header.set(false);
        obj
    }

    pub fn name(&self) -> String { self.imp().name.borrow().clone() }
    pub fn backend_type(&self) -> String { self.imp().backend_type.borrow().clone() }
    pub fn icon_name(&self) -> String { self.imp().icon_name.borrow().clone() }
    pub fn is_header(&self) -> bool { self.imp().is_header.get() }
}
