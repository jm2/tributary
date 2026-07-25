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
        /// Optional representative track + source identity used to fetch
        /// the album's artwork. Only the album pane populates this; genre
        /// and artist panes leave it empty so their lightweight labels
        /// carry no per-row art cost.
        pub artwork_candidate: RefCell<Option<super::AlbumArtCandidate>>,
        /// Whether this item is the synthetic "All" row. The "All" row is
        /// never decorated with artwork; the bind factory uses this to skip
        /// the artwork fetch path and stick to the existing text label.
        pub is_all_row: Cell<bool>,
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

/// Representative track + source identity used to fetch one album's
/// artwork asynchronously.
///
/// The browser pane stores at most one candidate per album item: the
/// first track whose artwork path the UI knows how to resolve. That keeps
/// the per-row memory bounded — every other track for the same album is
/// already covered by the same shared `Texture` once the cache warms up.
///
/// Stored inside `BrowserItem`, so the type must remain `Clone + 'static`
/// and never carry an open file handle. Local embedded extraction is
/// triggered through the URI string in the existing
/// `album_art::update_direct_file_album_art` path, which is display-only
/// (playback never reads artwork from a browser row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumArtCandidate {
    pub track_id: String,
    pub uri: String,
    pub cover_art_url: String,
    pub source_id: Option<crate::architecture::SourceId>,
    pub source_session_epoch: Option<u64>,
}

impl BrowserItem {
    pub fn new(label: &str, count: u32) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().label.replace(label.to_string());
        obj.imp().count.set(count);
        obj.imp().is_all_row.set(label == "All");
        obj
    }

    /// Construct an album-pane item with a representative artwork candidate.
    pub fn new_with_artwork(label: &str, count: u32, candidate: AlbumArtCandidate) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().label.replace(label.to_string());
        obj.imp().count.set(count);
        obj.imp().is_all_row.set(false);
        obj.imp().artwork_candidate.replace(Some(candidate));
        obj
    }

    pub fn label(&self) -> String {
        self.imp().label.borrow().clone()
    }
    pub fn count(&self) -> u32 {
        self.imp().count.get()
    }
    pub fn is_all_row(&self) -> bool {
        self.imp().is_all_row.get()
    }
    pub fn artwork_candidate(&self) -> Option<AlbumArtCandidate> {
        self.imp().artwork_candidate.borrow().clone()
    }

    pub fn display(&self) -> String {
        format!("{} ({})", self.label(), self.count())
    }
}
