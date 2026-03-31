//! `TrackObject` — GObject wrapper for displaying tracks in `GtkColumnView`.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::subclass::prelude::*;

// ---------------------------------------------------------------------------
// Inner (private) implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct TrackObject {
        pub track_number: Cell<u32>,
        pub title: RefCell<String>,
        pub duration_secs: Cell<u64>,
        pub artist: RefCell<String>,
        pub album: RefCell<String>,
        pub genre: RefCell<String>,
        pub year: Cell<i32>,
        pub date_modified: RefCell<String>,
        pub bitrate_kbps: Cell<u32>,
        pub sample_rate_hz: Cell<u32>,
        pub play_count: Cell<u32>,
        pub format: RefCell<String>,
        /// Playable URI (`file:///…` or stream URL).
        pub uri: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackObject {
        const NAME: &'static str = "TributaryTrackObject";
        type Type = super::TrackObject;
    }

    impl ObjectImpl for TrackObject {}
}

// ---------------------------------------------------------------------------
// Public wrapper
// ---------------------------------------------------------------------------
glib::wrapper! {
    pub struct TrackObject(ObjectSubclass<imp::TrackObject>);
}

impl TrackObject {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        track_number: u32,
        title: &str,
        duration_secs: u64,
        artist: &str,
        album: &str,
        genre: &str,
        year: i32,
        date_modified: &str,
        bitrate_kbps: u32,
        sample_rate_hz: u32,
        play_count: u32,
        format: &str,
        uri: &str,
    ) -> Self {
        let obj: Self = glib::Object::builder().build();
        let imp = obj.imp();
        imp.track_number.set(track_number);
        imp.title.replace(title.to_string());
        imp.duration_secs.set(duration_secs);
        imp.artist.replace(artist.to_string());
        imp.album.replace(album.to_string());
        imp.genre.replace(genre.to_string());
        imp.year.set(year);
        imp.date_modified.replace(date_modified.to_string());
        imp.bitrate_kbps.set(bitrate_kbps);
        imp.sample_rate_hz.set(sample_rate_hz);
        imp.play_count.set(play_count);
        imp.format.replace(format.to_string());
        imp.uri.replace(uri.to_string());
        obj
    }

    pub fn track_number(&self) -> u32 {
        self.imp().track_number.get()
    }
    pub fn title(&self) -> String {
        self.imp().title.borrow().clone()
    }
    pub fn duration_secs(&self) -> u64 {
        self.imp().duration_secs.get()
    }
    pub fn artist(&self) -> String {
        self.imp().artist.borrow().clone()
    }
    pub fn album(&self) -> String {
        self.imp().album.borrow().clone()
    }
    pub fn genre(&self) -> String {
        self.imp().genre.borrow().clone()
    }
    pub fn year(&self) -> i32 {
        self.imp().year.get()
    }
    pub fn date_modified(&self) -> String {
        self.imp().date_modified.borrow().clone()
    }
    pub fn bitrate_kbps(&self) -> u32 {
        self.imp().bitrate_kbps.get()
    }
    pub fn sample_rate_hz(&self) -> u32 {
        self.imp().sample_rate_hz.get()
    }
    pub fn play_count(&self) -> u32 {
        self.imp().play_count.get()
    }
    pub fn format(&self) -> String {
        self.imp().format.borrow().clone()
    }
    pub fn uri(&self) -> String {
        self.imp().uri.borrow().clone()
    }

    pub fn duration_display(&self) -> String {
        let secs = self.duration_secs();
        format!("{}:{:02}", secs / 60, secs % 60)
    }

    pub fn year_display(&self) -> String {
        let y = self.year();
        if y > 0 {
            y.to_string()
        } else {
            String::new()
        }
    }

    pub fn bitrate_display(&self) -> String {
        let b = self.bitrate_kbps();
        if b > 0 {
            format!("{b} kbps")
        } else {
            String::new()
        }
    }

    pub fn sample_rate_display(&self) -> String {
        let sr = self.sample_rate_hz();
        if sr > 0 {
            format!("{:.1} kHz", sr as f64 / 1000.0)
        } else {
            String::new()
        }
    }

    pub fn play_count_display(&self) -> String {
        let pc = self.play_count();
        if pc > 0 {
            pc.to_string()
        } else {
            String::new()
        }
    }
}
