//! `TrackObject` — GObject wrapper for displaying tracks in `GtkColumnView`.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

use gtk::glib;
use gtk::subclass::prelude::*;

use crate::architecture::models::TrackRating;
use crate::architecture::{SourceId, TrackId};
use crate::source_registry::RegularPlaylistCatalogueGuard;

static NEXT_ROW_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Closed, non-sensitive reason that one durable playlist occurrence cannot
/// currently supply a playable row.
///
/// Local states are kept separate because an imported occurrence that has
/// never matched a library track is different from one whose exact local
/// track disappeared. Remote states mirror the registry's closed result and
/// never retain backend errors, locators, or stale metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistRowUnavailableReason {
    LocalTrackMissing,
    LocalTrackUnmatched,
    SourceUnavailable,
    UnsupportedSource,
    InvalidCatalogue,
    TrackMissing,
}

/// Transient authority state attached to one rendered regular-playlist row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistOccurrenceState {
    AvailableLocal,
    AvailableRemote(RegularPlaylistCatalogueGuard),
    Unavailable(PlaylistRowUnavailableReason),
}

/// Exact durable occurrence identity plus its current presentation authority.
///
/// `entry_id` identifies the removable/reorderable occurrence. `source_id`
/// and `track_id` identify its media owner; a missing track ID is permitted
/// only for an unmatched local import. The remote catalogue guard is
/// transient and is never written back to playlist storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistOccurrenceBinding {
    entry_id: String,
    source_id: SourceId,
    track_id: Option<TrackId>,
    state: PlaylistOccurrenceState,
}

impl PlaylistOccurrenceBinding {
    fn new(
        entry_id: impl Into<String>,
        source_id: SourceId,
        track_id: Option<TrackId>,
        state: PlaylistOccurrenceState,
    ) -> Option<Self> {
        let entry_id = entry_id.into();
        if entry_id.is_empty() {
            return None;
        }

        let valid = match state {
            PlaylistOccurrenceState::AvailableLocal => {
                source_id == SourceId::local() && track_id.is_some()
            }
            PlaylistOccurrenceState::AvailableRemote(guard) => {
                source_id != SourceId::local()
                    && track_id.is_some()
                    && guard.source_id() == source_id
            }
            PlaylistOccurrenceState::Unavailable(
                PlaylistRowUnavailableReason::LocalTrackMissing,
            ) => source_id == SourceId::local() && track_id.is_some(),
            PlaylistOccurrenceState::Unavailable(
                PlaylistRowUnavailableReason::LocalTrackUnmatched,
            ) => source_id == SourceId::local() && track_id.is_none(),
            PlaylistOccurrenceState::Unavailable(
                PlaylistRowUnavailableReason::SourceUnavailable
                | PlaylistRowUnavailableReason::UnsupportedSource
                | PlaylistRowUnavailableReason::InvalidCatalogue
                | PlaylistRowUnavailableReason::TrackMissing,
            ) => source_id != SourceId::local() && track_id.is_some(),
        };
        valid.then_some(Self {
            entry_id,
            source_id,
            track_id,
            state,
        })
    }

    pub(crate) fn available_local(entry_id: impl Into<String>, track_id: TrackId) -> Option<Self> {
        Self::new(
            entry_id,
            SourceId::local(),
            Some(track_id),
            PlaylistOccurrenceState::AvailableLocal,
        )
    }

    pub(crate) fn available_remote(
        entry_id: impl Into<String>,
        source_id: SourceId,
        track_id: TrackId,
        guard: RegularPlaylistCatalogueGuard,
    ) -> Option<Self> {
        Self::new(
            entry_id,
            source_id,
            Some(track_id),
            PlaylistOccurrenceState::AvailableRemote(guard),
        )
    }

    pub(crate) fn unavailable(
        entry_id: impl Into<String>,
        source_id: SourceId,
        track_id: Option<TrackId>,
        reason: PlaylistRowUnavailableReason,
    ) -> Option<Self> {
        Self::new(
            entry_id,
            source_id,
            track_id,
            PlaylistOccurrenceState::Unavailable(reason),
        )
    }

    pub(crate) fn entry_id(&self) -> &str {
        &self.entry_id
    }

    pub(crate) const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub(crate) fn track_id(&self) -> Option<&TrackId> {
        self.track_id.as_ref()
    }

    pub(crate) const fn state(&self) -> PlaylistOccurrenceState {
        self.state
    }
}

// ---------------------------------------------------------------------------
// Inner (private) implementation
// ---------------------------------------------------------------------------
mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct TrackObject {
        /// Exact owner of this row's media identity. UI-only legacy rows may
        /// omit it; architecture-backed rows must set it explicitly.
        pub source_id: Cell<Option<SourceId>>,
        /// Durable regular-playlist occurrence plus its transient live state.
        pub(super) playlist_occurrence: RefCell<Option<PlaylistOccurrenceBinding>>,
        /// Stable backend-provided track identifier.  Sources without a
        /// native identifier fall back to the playable URI in `track_id()`.
        pub track_id: RefCell<String>,
        /// Distinguishes an explicitly supplied (even malformed/empty) native
        /// identifier from UI-only rows that deliberately use their URI.
        pub has_explicit_track_id: Cell<bool>,
        /// Non-secret lifecycle epoch that published a source-owned row.
        /// Zero means the row is not owned by a lifecycle session.
        pub source_session_epoch: Cell<u64>,
        /// Accepted source-wide catalogue generation that published the row.
        /// Zero means the row is not a catalogue observation.
        pub source_catalogue_generation: Cell<u64>,
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
        /// Canonical rating plus the capability of the publishing source.
        /// The fail-closed default is `Unsupported` for UI-only rows.
        pub rating: Cell<TrackRating>,
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
    pub fn rating(&self) -> TrackRating {
        self.imp().rating.get()
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

    /// Return the exact media owner carried by this row, when one has been
    /// assigned. This never derives identity from a URI or navigation key.
    pub(crate) fn source_id(&self) -> Option<SourceId> {
        self.imp().source_id.get()
    }

    /// Assign an exact row owner without allowing it to contradict an
    /// attached durable playlist occurrence.
    pub(crate) fn set_source_id(&self, source_id: SourceId) -> bool {
        if self
            .imp()
            .playlist_occurrence
            .borrow()
            .as_ref()
            .is_some_and(|binding| binding.source_id() != source_id)
        {
            return false;
        }
        self.imp().source_id.set(Some(source_id));
        true
    }

    pub(crate) fn playlist_occurrence_binding(&self) -> Option<PlaylistOccurrenceBinding> {
        self.imp().playlist_occurrence.borrow().clone()
    }

    /// Attach one validated playlist occurrence and make its source/native
    /// identity authoritative for this row. An unmatched local occurrence
    /// receives an explicit empty track ID so `track_id()` cannot fall back to
    /// a locator retained by a legacy constructor.
    pub(crate) fn set_playlist_occurrence_binding(&self, binding: PlaylistOccurrenceBinding) {
        self.imp().source_id.set(Some(binding.source_id()));
        self.set_track_id(binding.track_id().map_or("", TrackId::as_str));
        self.imp().playlist_occurrence.replace(Some(binding));
    }

    pub(crate) fn source_session_epoch(&self) -> Option<u64> {
        let epoch = self.imp().source_session_epoch.get();
        (epoch != 0).then_some(epoch)
    }

    pub(crate) fn set_source_session_epoch(&self, epoch: u64) {
        debug_assert_ne!(epoch, 0, "lifecycle session epochs are non-zero");
        self.imp().source_session_epoch.set(epoch);
    }

    pub(crate) fn source_catalogue_generation(&self) -> Option<u64> {
        let generation = self.imp().source_catalogue_generation.get();
        (generation != 0).then_some(generation)
    }

    pub(crate) fn set_source_catalogue_generation(&self, generation: u64) {
        debug_assert_ne!(generation, 0, "catalogue generations are non-zero");
        self.imp().source_catalogue_generation.set(generation);
    }

    pub fn set_album_artist(&self, name: &str) {
        self.imp().album_artist.replace(name.to_string());
    }

    pub fn set_disc_number(&self, disc: u32) {
        self.imp().disc_number.set(disc);
    }

    pub fn set_rating(&self, rating: TrackRating) {
        self.imp().rating.set(rating);
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
    use crate::architecture::models::Rating;

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
    fn playlist_binding_replaces_locator_fallback_with_exact_optional_identity() {
        let row = track("file:///private/reconciliation/evidence.flac");
        let binding = PlaylistOccurrenceBinding::unavailable(
            "unmatched-entry",
            SourceId::local(),
            None,
            PlaylistRowUnavailableReason::LocalTrackUnmatched,
        )
        .expect("valid unmatched local occurrence");

        row.set_playlist_occurrence_binding(binding.clone());

        assert_eq!(row.source_id(), Some(SourceId::local()));
        assert_eq!(row.track_id(), "");
        assert_eq!(row.playlist_occurrence_binding(), Some(binding));
    }

    #[test]
    fn duplicate_playlist_media_keeps_distinct_durable_occurrence_ids() {
        let track_id = TrackId::new("same-track").expect("track ID");
        let first_binding =
            PlaylistOccurrenceBinding::available_local("entry-one", track_id.clone())
                .expect("first occurrence");
        let duplicate_binding =
            PlaylistOccurrenceBinding::available_local("entry-two", track_id.clone())
                .expect("duplicate occurrence");
        let first = track("file:///same.flac");
        let duplicate = track("file:///same.flac");
        first.set_playlist_occurrence_binding(first_binding);
        duplicate.set_playlist_occurrence_binding(duplicate_binding);

        let first_binding = first.playlist_occurrence_binding().expect("first binding");
        let duplicate_binding = duplicate
            .playlist_occurrence_binding()
            .expect("duplicate binding");
        assert_eq!(first_binding.track_id(), Some(&track_id));
        assert_eq!(duplicate_binding.track_id(), Some(&track_id));
        assert_ne!(first_binding.entry_id(), duplicate_binding.entry_id());
        assert_ne!(first.row_instance_id(), duplicate.row_instance_id());
    }

    #[test]
    fn playlist_binding_state_and_source_invariants_fail_closed() {
        let local_track = TrackId::new("local-track").expect("local track ID");
        let remote_track = TrackId::remote("remote-track").expect("remote track ID");
        let remote_source = SourceId::remote(
            "subsonic",
            &url::Url::parse("https://music.example.test/").expect("remote URL"),
        )
        .expect("remote source");

        let missing = PlaylistOccurrenceBinding::unavailable(
            "missing-entry",
            SourceId::local(),
            Some(local_track.clone()),
            PlaylistRowUnavailableReason::LocalTrackMissing,
        )
        .expect("valid missing local occurrence");
        assert_eq!(
            missing.state(),
            PlaylistOccurrenceState::Unavailable(PlaylistRowUnavailableReason::LocalTrackMissing)
        );

        let unavailable = PlaylistOccurrenceBinding::unavailable(
            "remote-entry",
            remote_source,
            Some(remote_track.clone()),
            PlaylistRowUnavailableReason::SourceUnavailable,
        )
        .expect("valid unavailable remote occurrence");
        assert_eq!(unavailable.source_id(), remote_source);
        assert_eq!(unavailable.track_id(), Some(&remote_track));
        assert_eq!(
            unavailable.state(),
            PlaylistOccurrenceState::Unavailable(PlaylistRowUnavailableReason::SourceUnavailable)
        );

        assert!(PlaylistOccurrenceBinding::available_local("", local_track.clone()).is_none());
        assert!(PlaylistOccurrenceBinding::unavailable(
            "wrong-local-state",
            SourceId::local(),
            Some(local_track),
            PlaylistRowUnavailableReason::TrackMissing,
        )
        .is_none());
        assert!(PlaylistOccurrenceBinding::unavailable(
            "wrong-remote-state",
            remote_source,
            Some(remote_track),
            PlaylistRowUnavailableReason::LocalTrackMissing,
        )
        .is_none());
    }

    #[test]
    fn source_catalogue_observation_preserves_exact_nonzero_values() {
        let row = track("");
        assert_eq!(row.source_session_epoch(), None);
        assert_eq!(row.source_catalogue_generation(), None);

        row.set_source_session_epoch(17);
        row.set_source_catalogue_generation(29);
        assert_eq!(row.source_session_epoch(), Some(17));
        assert_eq!(row.source_catalogue_generation(), Some(29));
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

    #[test]
    fn rating_projection_defaults_fail_closed_and_preserves_every_state() {
        let row = track("file:///music/rating.flac");
        assert_eq!(row.rating(), TrackRating::unsupported());

        let value = Rating::new(73).unwrap();
        for rating in [
            TrackRating::unsupported(),
            TrackRating::read_only(None),
            TrackRating::read_only(Some(value)),
            TrackRating::writable(None),
            TrackRating::writable(Some(value)),
        ] {
            row.set_rating(rating);
            assert_eq!(row.rating(), rating);
        }
    }
}
