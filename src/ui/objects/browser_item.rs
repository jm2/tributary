//! `BrowserItem` — GObject wrapper for genre / artist / album browser panes.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct BrowserItem {
        pub label: RefCell<String>,
        pub count: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BrowserItem {
        const NAME: &'static str = "TributaryBrowserItem";
        type Type = super::BrowserItem;
    }

    impl ObjectImpl for BrowserItem {}
}

glib::wrapper! {
    pub struct BrowserItem(ObjectSubclass<imp::BrowserItem>);
}

impl BrowserItem {
    pub fn new(label: &str, count: u32) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().label.replace(label.to_string());
        obj.imp().count.set(count);
        obj
    }

    pub fn label(&self) -> String {
        self.imp().label.borrow().clone()
    }
    pub fn count(&self) -> u32 {
        self.imp().count.get()
    }

    pub fn display(&self) -> String {
        format!("{} ({})", self.label(), self.count())
    }
}
