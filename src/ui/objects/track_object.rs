//! `TrackObject` — GObject wrapper for displaying tracks in `GtkColumnView`.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

use gtk::glib;
use gtk::subclass::prelude::*;

static NEXT_ROW_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Inner (private) implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct TrackObject {
        /// Stable backend-provided track identifier.  Sources without a
        /// native identifier fall back to the playable URI in `track_id()`.
        pub track_id: RefCell<String>,
        /// Distinguishes an explicitly supplied (even malformed/empty) native
        /// identifier from UI-only rows that deliberately use their URI.
        pub has_explicit_track_id: Cell<bool>,
        /// Non-secret lifecycle epoch that published a source-owned row.
        /// Zero means the row is not owned by a lifecycle session.
        pub source_session_epoch: Cell<u64>,
        /// Identity of this concrete UI row instance. Duplicate playlist
        /// entries share `track_id` but receive distinct row IDs.
        pub row_instance_id: Cell<u64>,
        pub track_number: Cell<u32>,
        /// Disc number (0 = unset, mirrors `Track::disc_number == None`).
        pub disc_number: Cell<u32>,
        pub title: RefCell<String>,
        pub duration_secs: Cell<u64>,
        pub artist: RefCell<String>,
        pub album_artist: RefCell<String>,
        pub album: RefCell<String>,
        pub genre: RefCell<String>,
        pub composer: RefCell<String>,
        pub year: Cell<i32>,
        pub date_modified: RefCell<String>,
        pub bitrate_kbps: Cell<u32>,
        pub sample_rate_hz: Cell<u32>,
        pub play_count: Cell<u32>,
        pub format: RefCell<String>,
        /// Playable URI (`file:///…` or stream URL).
        pub uri: RefCell<String>,
        /// Cover art URL for remote tracks (empty for local files).
        pub cover_art_url: RefCell<String>,
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
        composer: &str,
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
        imp.row_instance_id
            .set(NEXT_ROW_INSTANCE_ID.fetch_add(1, Ordering::Relaxed));
        imp.track_number.set(track_number);
        imp.title.replace(title.to_string());
        imp.duration_secs.set(duration_secs);
        imp.artist.replace(artist.to_string());
        imp.album.replace(album.to_string());
        imp.genre.replace(genre.to_string());
        imp.composer.replace(composer.to_string());
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
    /// Stable identifier used by playback sessions.
    ///
    /// Architecture-backed tracks and UI adapters set their exact native ID
    /// explicitly. The URI fallback exists only for legacy row constructors;
    /// queue admission validates a non-empty bounded ID before publication.
    pub fn track_id(&self) -> String {
        let imp = self.imp();
        if imp.has_explicit_track_id.get() {
            imp.track_id.borrow().clone()
        } else {
            self.uri()
        }
    }
    /// Identity used only to reselect this concrete queue occurrence in GTK.
    /// It is deliberately separate from the stable backend track identity.
    pub fn row_instance_id(&self) -> u64 {
        let id = self.imp().row_instance_id.get();
        if id != 0 {
            return id;
        }
        // Be defensive for objects constructed through the raw GObject
        // builder rather than `TrackObject::new`.
        let id = NEXT_ROW_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        self.imp().row_instance_id.set(id);
        id
    }
    pub fn disc_number(&self) -> u32 {
        self.imp().disc_number.get()
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
    pub fn album_artist(&self) -> String {
        self.imp().album_artist.borrow().clone()
    }
    pub fn album(&self) -> String {
        self.imp().album.borrow().clone()
    }
    pub fn genre(&self) -> String {
        self.imp().genre.borrow().clone()
    }
    pub fn composer(&self) -> String {
        self.imp().composer.borrow().clone()
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
    pub fn cover_art_url(&self) -> String {
        self.imp().cover_art_url.borrow().clone()
    }

    pub fn set_cover_art_url(&self, url: &str) {
        self.imp().cover_art_url.replace(url.to_string());
    }

    /// Retarget an existing UI row without replacing its occurrence identity.
    /// The URI is not a displayed GObject property, so no notify signal is
    /// required; future playback reads it directly from this row.
    pub(crate) fn set_uri(&self, uri: &str) {
        self.imp().uri.replace(uri.to_string());
    }

    pub fn set_track_id(&self, id: &str) {
        let imp = self.imp();
        imp.track_id.replace(id.to_string());
        imp.has_explicit_track_id.set(true);
    }

    pub(crate) fn source_session_epoch(&self) -> Option<u64> {
        let epoch = self.imp().source_session_epoch.get();
        (epoch != 0).then_some(epoch)
    }

    pub(crate) fn set_source_session_epoch(&self, epoch: u64) {
        debug_assert_ne!(epoch, 0, "lifecycle session epochs are non-zero");
        self.imp().source_session_epoch.set(epoch);
    }

    pub fn set_album_artist(&self, name: &str) {
        self.imp().album_artist.replace(name.to_string());
    }

    pub fn set_disc_number(&self, disc: u32) {
        self.imp().disc_number.set(disc);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn track(uri: &str) -> TrackObject {
        TrackObject::new(
            1, "Title", 60, "Artist", "Album", "", "", 0, "", 0, 0, 0, "", uri,
        )
    }

    #[test]
    fn duplicate_track_rows_have_distinct_instance_identity() {
        let first = track("file:///same.flac");
        let duplicate = track("file:///same.flac");
        first.set_track_id("stable-track-id");
        duplicate.set_track_id("stable-track-id");

        assert_eq!(first.track_id(), duplicate.track_id());
        assert_ne!(first.row_instance_id(), duplicate.row_instance_id());
        assert_eq!(first.row_instance_id(), first.clone().row_instance_id());
    }

    #[test]
    fn an_explicit_empty_native_id_never_falls_back_to_a_file_locator() {
        let row = track("file:///private/library/track.flac");
        assert_eq!(row.track_id(), "file:///private/library/track.flac");

        row.set_track_id("");
        assert_eq!(
            row.track_id(),
            "",
            "malformed persisted identity must fail resolution, not become a path identity"
        );
    }
}
