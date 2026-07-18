//! Playback context and track navigation helpers.
//!
//! This module provides:
//! - [`PlaybackContext`] — shared state passed to playback functions
//! - [`play_track_at`] — load and play a specific track by position
//! - [`advance_track`] — move to the next track (shuffle/repeat aware)
//! - [`format_ms`] — format milliseconds as `m:ss` or `h:mm:ss`

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::warn;

use crate::architecture::{MediaKey, SourceId, TrackId, ViewOrigin};
use crate::audio::output::AudioOutput;
use crate::audio::PlayerEventGeneration;
use crate::ui::header_bar::RepeatMode;
use crate::ui::objects::TrackObject;

use super::album_art;

/// The source key of the local library.
pub const LOCAL_SOURCE_KEY: &str = "local";

/// The source-key prefix of every playlist view. Playlists are projections of
/// the local library, so their queue items carry library track IDs too.
pub const PLAYLIST_SOURCE_PREFIX: &str = "playlist:";

/// Whether a queue source is backed by the local library database, and its
/// track IDs are therefore library track IDs.
///
/// Remote backends key tracks by their own native IDs, removable tracks use a
/// lossless mount-relative ID, and external sessions mint ephemeral IDs. A
/// library update must never reinterpret one of those as one of its own.
fn is_library_source(source_id: SourceId) -> bool {
    source_id == SourceId::local()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueueView {
    source_id: SourceId,
    origin: Option<ViewOrigin>,
}

fn queue_view(source_key: &str) -> Option<QueueView> {
    if source_key == LOCAL_SOURCE_KEY {
        return Some(QueueView {
            source_id: SourceId::local(),
            origin: None,
        });
    }
    if let Some(playlist_id) = source_key.strip_prefix(PLAYLIST_SOURCE_PREFIX) {
        return Some(QueueView {
            source_id: SourceId::local(),
            origin: Some(ViewOrigin::playlist(playlist_id).ok()?),
        });
    }
    if source_key.starts_with("radio-") {
        return Some(QueueView {
            source_id: SourceId::radio_browser(),
            origin: Some(ViewOrigin::radio(source_key).ok()?),
        });
    }
    if let Ok(source_id) = source_key.parse::<SourceId>() {
        return Some(QueueView {
            source_id,
            origin: None,
        });
    }
    Some(QueueView {
        source_id: SourceId::removable(source_key).ok()?,
        origin: None,
    })
}

fn identity_belongs_to_source(identity: &PlaybackIdentity, source_key: &str) -> bool {
    if source_key == LOCAL_SOURCE_KEY {
        return identity.media_key.source_id == SourceId::local();
    }
    if let Some(playlist_id) = source_key.strip_prefix(PLAYLIST_SOURCE_PREFIX) {
        return identity.view_origin == Some(ViewOrigin::Playlist(playlist_id.to_string()));
    }
    if source_key.starts_with("radio-") {
        return identity.view_origin == Some(ViewOrigin::Radio(source_key.to_string()));
    }
    identity.view_origin.is_none()
        && queue_view(source_key).is_some_and(|view| view.source_id == identity.media_key.source_id)
}

/// Whether `source_key` names the media-source session that owns this item.
///
/// Playlist and radio-feed keys are view origins, not source owners. They are
/// intentionally accepted by `identity_belongs_to_source` for GTK visibility,
/// but retiring one view must not revoke media owned by the local library or
/// shared Radio-Browser source.
fn identity_is_owned_by_source(identity: &PlaybackIdentity, source_key: &str) -> bool {
    let source_id = if source_key == LOCAL_SOURCE_KEY {
        Some(SourceId::local())
    } else if source_key.starts_with(PLAYLIST_SOURCE_PREFIX) || source_key.starts_with("radio-") {
        None
    } else if let Ok(source_id) = source_key.parse::<SourceId>() {
        Some(source_id)
    } else {
        SourceId::removable(source_key).ok()
    };
    source_id == Some(identity.media_key.source_id)
}

/// Overlay committed local-library URIs onto an existing playlist projection.
///
/// Playlist rows and the local library share stable track IDs, but each
/// playlist occurrence owns a distinct row identity. Mutating only the URI in
/// place preserves duplicate ordering and selection while ensuring a later
/// click builds its queue from the committed path. Empty replacement URIs are
/// ignored so a transiently unplayable update cannot strand a valid row.
pub(super) fn refresh_projected_library_uris(
    projected_rows: &[TrackObject],
    committed_local_rows: &[TrackObject],
) -> usize {
    if projected_rows.is_empty() || committed_local_rows.is_empty() {
        return 0;
    }

    let projected_ids: HashSet<String> = projected_rows.iter().map(TrackObject::track_id).collect();
    let mut committed_uris = HashMap::with_capacity(projected_ids.len());
    for track in committed_local_rows {
        let track_id = track.track_id();
        if !projected_ids.contains(&track_id) {
            continue;
        }
        let uri = track.uri();
        if !uri.is_empty() {
            committed_uris.insert(track_id, uri);
        }
    }

    let mut refreshed = 0;
    for row in projected_rows {
        let Some(uri) = committed_uris.get(&row.track_id()) else {
            continue;
        };
        if row.uri() != *uri {
            row.set_uri(uri);
            refreshed += 1;
        }
    }
    refreshed
}

/// Stable media identity plus the view that supplied this queue occurrence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackIdentity {
    pub media_key: MediaKey,
    pub view_origin: Option<ViewOrigin>,
}

impl PlaybackIdentity {
    fn new(view: &QueueView, track_id: TrackId) -> Self {
        Self {
            media_key: MediaKey::new(view.source_id, track_id),
            view_origin: view.origin.clone(),
        }
    }
}

/// A committed library change, addressed to the queue by stable track ID.
///
/// A rename moves a track's file without changing what it is, so the queue must
/// re-resolve where to play it from while keeping the identity, position, and
/// history it captured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTrackRefresh {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_art_url: String,
}

impl QueueTrackRefresh {
    pub fn from_track(track: &TrackObject) -> Self {
        Self {
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            cover_art_url: track.cover_art_url(),
        }
    }
}

/// Immutable metadata captured when a playback queue is created.
///
/// Keeping this snapshot outside GTK's mutable sort/filter models means view
/// changes cannot silently retarget Next, Previous, repeat, or EOS handling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueItem {
    pub identity: PlaybackIdentity,
    /// Zero-based occurrence of this stable track identity in the captured
    /// source queue. Playlist entries may intentionally reference one track
    /// more than once; this disambiguates row selection without changing the
    /// stable source/track identity used for playback ownership.
    occurrence: usize,
    /// Concrete GTK row captured with the queue. This distinguishes duplicate
    /// playlist entries while that source model is alive; `occurrence` remains
    /// a fallback if the view is rebuilt with fresh row objects.
    row_instance_id: Option<u64>,
    /// Exact authenticated-remote session epoch that published this row.
    /// Stable media identity remains `(SourceId, TrackId)`; the epoch prevents
    /// a captured queue from being retargeted to a replacement login.
    session_epoch: Option<u64>,
    uri: String,
    title: String,
    artist: String,
    album: String,
    cover_art_url: String,
}

impl QueueItem {
    fn from_track(identity: PlaybackIdentity, track: &TrackObject, occurrence: usize) -> Self {
        let is_library = is_library_source(identity.media_key.source_id);
        Self {
            identity,
            occurrence,
            row_instance_id: Some(track.row_instance_id()),
            session_epoch: track.remote_session_epoch(),
            // Local and playlist queues retain identity, ordering, and a
            // metadata snapshot but no file locator. Every output load
            // resolves the exact database ID against the current row.
            uri: if is_library {
                String::new()
            } else {
                track.uri()
            },
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            cover_art_url: track.cover_art_url(),
        }
    }

    pub(crate) fn external(uri: String, title: String, artist: String, album: String) -> Self {
        let source_id = SourceId::external();
        let track_id = TrackId::external();
        Self {
            identity: PlaybackIdentity {
                media_key: MediaKey::new(source_id, track_id),
                view_origin: None,
            },
            occurrence: 0,
            row_instance_id: None,
            session_epoch: None,
            uri,
            title,
            artist,
            album,
            cover_art_url: String::new(),
        }
    }

    #[cfg(test)]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    #[cfg(test)]
    pub(crate) const fn session_epoch(&self) -> Option<u64> {
        self.session_epoch
    }
}

#[derive(Clone, Debug)]
struct ShuffleState {
    /// Items visited in this shuffle cycle, ending with the current item.
    history: Vec<usize>,
    /// Items not yet visited in this cycle.
    remaining: Vec<usize>,
}

/// Playback-owned queue and cursor.
///
/// A session is replaced only when the user explicitly starts a track (or an
/// external file), reaches the unrepeated end, stops playback, or changes the
/// output target. Sorting, filtering, and sidebar navigation never mutate it.
/// Sequential Next/Previous follow snapshot order; repeat-all wraps that
/// snapshot. Shuffle visits every snapshot item once per cycle, Previous walks
/// shuffle history, and repeat-all starts a new shuffled cycle. Repeat-one is
/// an EOS policy implemented by [`replay_current`], so manual Next still moves.
#[derive(Clone, Debug, Default)]
pub struct PlaybackSession {
    queue: Vec<QueueItem>,
    current_index: Option<usize>,
    shuffle: Option<ShuffleState>,
    event_generation: PlayerEventGeneration,
    /// A protected source reference is being resolved for this generation.
    /// Play/toggle requests are accepted as no-ops until the exact request is
    /// handed to the output, so they cannot revive a superseded track.
    pending_resolution: Option<PlayerEventGeneration>,
    /// The current item failed before reaching an output or was synchronously
    /// rejected by it. A later Play/Toggle retries the load (and protected
    /// resolution when applicable) instead of issuing `play()` to an output
    /// that has no loaded media.
    resolution_failed: bool,
}

impl PlaybackSession {
    pub(crate) fn replace_queue(&mut self, queue: Vec<QueueItem>, start_index: usize) -> bool {
        if queue.get(start_index).is_none() {
            return false;
        }
        self.queue = queue;
        self.current_index = Some(start_index);
        self.shuffle = None;
        self.pending_resolution = None;
        self.resolution_failed = false;
        true
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.current_index = None;
        self.shuffle = None;
        self.event_generation = self.event_generation.next();
        self.pending_resolution = None;
        self.resolution_failed = false;
    }

    /// Clear playback only when the current queue belongs to `source_id`.
    ///
    /// Remote source replacement and removal retire every opaque reference
    /// captured by that queue. Keeping it would strand Next/Previous on a
    /// revoked lease, while retargeting it could cross into a different login
    /// or library that reused the same server-native track identifiers.
    pub(crate) fn clear_if_source(&mut self, source_id: &str) -> bool {
        if self
            .current_identity()
            .is_none_or(|identity| !identity_is_owned_by_source(identity, source_id))
        {
            return false;
        }
        self.clear();
        true
    }

    /// Stable local-library IDs currently retained by the queue.
    ///
    /// A full library snapshot can be very large. Publishing this small set
    /// lets the GTK receiver avoid cloning refresh metadata for tracks the
    /// queue does not own, while preserving source namespacing.
    pub(crate) fn library_track_ids(&self) -> HashSet<&TrackId> {
        self.queue
            .iter()
            .filter(|item| is_library_source(item.identity.media_key.source_id))
            .map(|item| &item.identity.media_key.track_id)
            .collect()
    }

    /// Refresh display metadata for queued library identities whose current
    /// database row changed.
    ///
    /// File locations are deliberately absent from local/playlist queue items;
    /// playback resolves the current row by ID at every load. This update can
    /// therefore change only the metadata snapshot, never install a locator.
    ///
    /// Items are rewritten in place. Queue length, order, and the cursor are the
    /// coordinate system that `current_index` and the shuffle history index
    /// into, so identity — not position — is what an update may address.
    pub(crate) fn refresh_library_tracks(
        &mut self,
        updates: &HashMap<TrackId, QueueTrackRefresh>,
    ) -> usize {
        if updates.is_empty() {
            return 0;
        }

        let mut refreshed = 0;
        for item in &mut self.queue {
            if !is_library_source(item.identity.media_key.source_id) {
                continue;
            }
            let Some(update) = updates.get(&item.identity.media_key.track_id) else {
                continue;
            };
            if item.title == update.title
                && item.artist == update.artist
                && item.album == update.album
                && item.cover_art_url == update.cover_art_url
            {
                continue;
            }

            item.title = update.title.clone();
            item.artist = update.artist.clone();
            item.album = update.album.clone();
            item.cover_art_url = update.cover_art_url.clone();
            refreshed += 1;
        }

        refreshed
    }

    pub fn has_current(&self) -> bool {
        self.current().is_some()
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current_index.and_then(|index| self.queue.get(index))
    }

    pub fn current_identity(&self) -> Option<&PlaybackIdentity> {
        self.current().map(|item| &item.identity)
    }

    fn begin_event_generation(&mut self) -> PlayerEventGeneration {
        self.event_generation = self.event_generation.next();
        self.pending_resolution = None;
        self.resolution_failed = false;
        self.event_generation
    }

    /// Hand the current direct item to an output under a fresh event owner.
    ///
    /// This is the production boundary shared by initial playback, queue
    /// navigation, EOS replay, and a retry after synchronous rejection. The
    /// queue cursor supplies the URI; callers cannot accidentally load a row
    /// from the mutable GTK projection instead. A rejected load retains that
    /// exact item and generation as retryable, while a later attempt advances
    /// ownership before it calls the output again.
    pub(super) fn load_current_direct(
        &mut self,
        output: &dyn AudioOutput,
    ) -> Option<DirectLoadAttempt> {
        let uri = self.current()?.uri.clone();
        if uri.is_empty() || self.current()?.session_epoch.is_some() {
            return None;
        }

        let generation = self.begin_event_generation();
        output.set_event_generation(generation);
        let accepted = output.load_uri(&uri);
        if !accepted {
            let marked = self.mark_load_rejected(generation);
            debug_assert!(marked, "current direct load remains retryable");
        }
        Some(DirectLoadAttempt {
            generation,
            accepted,
        })
    }

    fn begin_pending_resolution(&mut self) -> PlayerEventGeneration {
        let generation = self.begin_event_generation();
        self.pending_resolution = Some(generation);
        self.resolution_failed = false;
        generation
    }

    /// Claim a completed resolution only while its queue item/generation still
    /// owns playback. Stop, Next, output replacement, and a newer replay all
    /// invalidate this proof before they can reach the output boundary.
    fn finish_pending_resolution(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution != Some(generation) || !self.accepts_event_generation(generation)
        {
            return false;
        }
        self.pending_resolution = None;
        self.resolution_failed = false;
        true
    }

    fn fail_pending_resolution(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution != Some(generation) || !self.accepts_event_generation(generation)
        {
            return false;
        }
        self.pending_resolution = None;
        self.resolution_failed = true;
        true
    }

    /// Cancel the current protected resolution while keeping its queue item
    /// available for a later Play retry.
    ///
    /// No media has reached the output yet, so pausing the output cannot
    /// preserve the user's intent. Clearing the exact pending claim makes its
    /// async completion a no-op; `resolution_failed` routes the next Play
    /// through a fresh playback-time resolution instead of calling `play()` on
    /// an empty output.
    pub(crate) fn cancel_pending_resolution_for_retry(&mut self) -> bool {
        if !self.is_resolution_pending() {
            return false;
        }
        self.pending_resolution = None;
        self.resolution_failed = true;
        true
    }

    /// Retain a synchronously rejected item as an explicit retryable load.
    ///
    /// The generation remains current so the output's required actionable
    /// error is still displayed. A later Play begins a fresh generation and
    /// attempts the load again instead of toggling an empty output session.
    fn mark_load_rejected(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some() || !self.accepts_event_generation(generation) {
            return false;
        }
        self.resolution_failed = true;
        true
    }

    /// Turn an ID/session-resolved output failure into a fresh-resolution retry
    /// state.
    ///
    /// Resolution has already finished by the time a protected request or
    /// exact local file URI reaches an output, so proxy startup, daemon,
    /// receiver, decoder, or filesystem failures arrive through
    /// `PlayerEvent::Error`. Without this transition, a later Play would call
    /// `play()` on an output that may never have accepted media instead of
    /// resolving the identity again. Advancing the event generation also
    /// rejects delayed state emitted by the failed load.
    pub(crate) fn mark_resolved_load_failed(&mut self, generation: PlayerEventGeneration) -> bool {
        if self.pending_resolution.is_some()
            // A synchronously rejected load is already retryable and never
            // created output state to stop. Keep its generation current so
            // the queued guidance remains visible without issuing cleanup.
            || self.resolution_failed
            || !self.accepts_event_generation(generation)
            || !self.current().is_some_and(|item| {
                is_library_source(item.identity.media_key.source_id)
                    || item.session_epoch.is_some()
            })
        {
            return false;
        }

        self.event_generation = self.event_generation.next();
        self.resolution_failed = true;
        true
    }

    fn is_resolution_pending(&self) -> bool {
        self.pending_resolution == Some(self.event_generation) && self.has_current()
    }

    pub fn accepts_event_generation(&self, generation: PlayerEventGeneration) -> bool {
        self.has_current() && self.event_generation == generation
    }

    fn initialize_shuffle(&mut self) {
        let Some(current) = self.current_index else {
            return;
        };
        let mut remaining: Vec<usize> = (0..self.queue.len())
            .filter(|&index| index != current)
            .collect();
        fastrand::shuffle(&mut remaining);
        self.shuffle = Some(ShuffleState {
            history: vec![current],
            remaining,
        });
    }

    fn advance(&mut self, repeat_mode: RepeatMode, shuffle: bool) -> Option<usize> {
        let current = self.current_index?;

        if !shuffle {
            self.shuffle = None;
            let next = current + 1;
            let selected = if next < self.queue.len() {
                next
            } else if repeat_mode == RepeatMode::All {
                0
            } else {
                return None;
            };
            self.current_index = Some(selected);
            self.pending_resolution = None;
            self.resolution_failed = false;
            return Some(selected);
        }

        if self.shuffle.is_none() {
            self.initialize_shuffle();
        }

        let state = self.shuffle.as_mut()?;
        if state.remaining.is_empty() {
            if repeat_mode != RepeatMode::All {
                return None;
            }
            state.remaining = (0..self.queue.len())
                .filter(|&index| index != current)
                .collect();
            fastrand::shuffle(&mut state.remaining);

            // A one-item queue repeats itself under repeat-all.
            if state.remaining.is_empty() {
                self.pending_resolution = None;
                self.resolution_failed = false;
                return Some(current);
            }
        }

        let selected = state.remaining.pop()?;
        state.history.push(selected);
        self.current_index = Some(selected);
        self.pending_resolution = None;
        self.resolution_failed = false;
        Some(selected)
    }

    fn previous(&mut self, repeat_mode: RepeatMode, shuffle: bool) -> Option<usize> {
        let current = self.current_index?;

        if !shuffle {
            self.shuffle = None;
            let selected = if current > 0 {
                current - 1
            } else if repeat_mode == RepeatMode::All {
                self.queue.len().checked_sub(1)?
            } else {
                return None;
            };
            self.current_index = Some(selected);
            self.pending_resolution = None;
            self.resolution_failed = false;
            return Some(selected);
        }

        if self.shuffle.is_none() {
            self.initialize_shuffle();
        }
        let state = self.shuffle.as_mut()?;
        if state.history.len() > 1 {
            if let Some(departed) = state.history.pop() {
                state.remaining.push(departed);
            }
            let selected = *state.history.last()?;
            self.current_index = Some(selected);
            self.pending_resolution = None;
            self.resolution_failed = false;
            return Some(selected);
        }

        if repeat_mode != RepeatMode::All {
            return None;
        }

        let mut candidates: Vec<usize> = (0..self.queue.len())
            .filter(|&index| index != current)
            .collect();
        fastrand::shuffle(&mut candidates);
        let selected = candidates.pop().unwrap_or(current);
        state.history = vec![selected];
        state.remaining = candidates;
        self.current_index = Some(selected);
        self.pending_resolution = None;
        self.resolution_failed = false;
        Some(selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectLoadAttempt {
    pub(super) generation: PlayerEventGeneration,
    pub(super) accepted: bool,
}

#[derive(Debug)]
struct CapturedQueue {
    items: Vec<QueueItem>,
    selected_index: usize,
}

/// Capture the current sorted/filtered projection as a playback-owned queue.
///
/// All entry points that start playback from the track list go through this
/// function. Once returned, the queue has no dependency on the mutable GTK
/// model: sorting, filtering, rebuilding, or navigating to another source can
/// no longer change the current identity or the meaning of Next/Previous.
fn capture_visible_queue(
    model: &impl IsA<gtk::gio::ListModel>,
    source_key: &str,
    selected_position: u32,
) -> Option<CapturedQueue> {
    let view = queue_view(source_key)?;
    let mut selected_index = None;
    let mut items = Vec::with_capacity(model.n_items() as usize);
    let mut occurrences: HashMap<MediaKey, usize> = HashMap::new();

    for model_index in 0..model.n_items() {
        let Some(track) = model.item(model_index).and_downcast::<TrackObject>() else {
            continue;
        };
        let Ok(track_id) = TrackId::new(track.track_id()) else {
            continue;
        };
        if model_index == selected_position {
            selected_index = Some(items.len());
        }
        let identity = PlaybackIdentity::new(&view, track_id);
        let occurrence = occurrences.entry(identity.media_key.clone()).or_default();
        items.push(QueueItem::from_track(identity, &track, *occurrence));
        *occurrence += 1;
    }

    Some(CapturedQueue {
        items,
        selected_index: selected_index?,
    })
}

/// Retire queue/event ownership before asking the active output to stop.
///
/// Both the explicit Stop control and output replacement use this ordering, so
/// a backend that publishes `Stopped` synchronously cannot mutate the cleared
/// or replacement session.
pub(super) fn stop_owned_playback(session: &mut PlaybackSession, output: &dyn AudioOutput) {
    session.clear();
    output.stop();
}

/// Debounce state for the play-button buffering spinner.
///
/// A reset or newer player state increments the token. Timeout callbacks keep
/// their original token and therefore become harmless no-ops when playback is
/// stopped, reaches EOS, changes output, or advances to another track.
#[derive(Debug, Default)]
pub struct BufferingTracker {
    generation: Cell<u64>,
    buffering: Cell<bool>,
}

impl BufferingTracker {
    pub fn begin(&self) -> u64 {
        let generation = self.invalidate();
        self.buffering.set(true);
        generation
    }

    pub fn invalidate(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.buffering.set(false);
        generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.buffering.get() && self.generation.get() == generation
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering.get()
    }
}

/// Shared state for playback operations.
///
/// Passed to [`play_track_at`] and [`advance_track`] so they can load
/// tracks, update the now-playing UI, and track the current position.
pub struct PlaybackContext {
    pub model: gtk::SortListModel,
    pub active_source_key: Rc<RefCell<String>>,
    pub active_output: Rc<RefCell<Box<dyn AudioOutput>>>,
    pub album_art: gtk::Image,
    pub title_label: gtk::Label,
    pub artist_label: gtk::Label,
    pub media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>>,
    pub session: Rc<RefCell<PlaybackSession>>,
    /// Current in-process configuration. Local resolution snapshots its roots
    /// before background work and rechecks them immediately before output.
    pub app_config: Rc<RefCell<super::preferences::AppConfig>>,
    /// Tokio runtime used for exact local-database resolution without
    /// blocking GTK's main thread.
    pub rt_handle: tokio::runtime::Handle,
    /// Sole authenticated-remote at-use resolver and lifecycle authority.
    pub remote_sources: crate::source_registry::RemoteSourceRegistry,
    /// The tracklist `ColumnView` — used to scroll the currently
    /// playing row into view on track change so the user doesn't lose
    /// their place when sequential / shuffled playback advances.
    pub column_view: gtk::ColumnView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayRequest {
    Resume,
    StartAt(u32),
    Unavailable,
}

fn resolve_play_request(has_current: bool, item_count: u32, shuffle: bool) -> PlayRequest {
    if has_current {
        return PlayRequest::Resume;
    }
    if item_count == 0 {
        return PlayRequest::Unavailable;
    }
    PlayRequest::StartAt(if shuffle {
        fastrand::u32(..item_count)
    } else {
        0
    })
}

/// Read the session behind a function boundary so its `Ref` is released before
/// the caller handles a request that may replace the queue.
fn resolve_session_play_request(
    session: &RefCell<PlaybackSession>,
    item_count: u32,
    shuffle: bool,
) -> PlayRequest {
    resolve_play_request(session.borrow().has_current(), item_count, shuffle)
}

/// Try to play the track at `position` in the given model.
///
/// Captures the visible sorted model as an immutable playback queue, then
/// starts the selected item. Later view mutations do not alter that queue.
pub fn play_track_at(position: u32, ctx: &PlaybackContext) -> bool {
    let source_key = ctx.active_source_key.borrow().clone();
    let Some(captured) = capture_visible_queue(&ctx.model, &source_key, position) else {
        return false;
    };
    let selected = &captured.items[captured.selected_index];
    if selected.uri.is_empty()
        && !is_library_source(selected.identity.media_key.source_id)
        && selected.session_epoch.is_none()
    {
        warn!("Track has no playable URI");
        return false;
    }

    let previous = ctx.session.borrow().clone();
    if !ctx
        .session
        .borrow_mut()
        .replace_queue(captured.items, captured.selected_index)
    {
        return false;
    }

    if play_current(ctx) {
        true
    } else {
        *ctx.session.borrow_mut() = previous;
        false
    }
}

/// Resume the session's current item, or create a new queue from the visible
/// model when playback is idle (including after an OS Stop action).
pub fn play_or_start(ctx: &PlaybackContext, shuffle: bool) -> bool {
    if ctx.session.borrow().is_resolution_pending() {
        return true;
    }
    if ctx.session.borrow().resolution_failed {
        return play_current(ctx);
    }
    // Do not borrow the RefCell directly in the match scrutinee: scrutinee
    // temporaries live through the selected arm, and StartAt mutably borrows
    // the same session while installing the newly captured queue.
    match resolve_session_play_request(&ctx.session, ctx.model.n_items(), shuffle) {
        PlayRequest::Resume => {
            ctx.active_output.borrow().play();
            true
        }
        PlayRequest::StartAt(position) => play_track_at(position, ctx),
        PlayRequest::Unavailable => false,
    }
}

/// Header/OS-toggle behavior: toggle a loaded item, otherwise start a queue.
pub fn toggle_or_start(ctx: &PlaybackContext, shuffle: bool) -> bool {
    if ctx.session.borrow().is_resolution_pending() {
        true
    } else if ctx.session.borrow().resolution_failed {
        play_current(ctx)
    } else if ctx.session.borrow().has_current() {
        ctx.active_output.borrow().toggle_play_pause();
        true
    } else {
        play_or_start(ctx, shuffle)
    }
}

/// Invalidate the session before stopping the output so synchronously emitted
/// Stopped events are already stale. The caller owns the widget reset.
pub fn stop_playback(ctx: &PlaybackContext) {
    let output = ctx.active_output.borrow();
    stop_owned_playback(&mut ctx.session.borrow_mut(), output.as_ref());
}

/// Load the current immutable queue item and refresh now-playing UI.
fn play_current(ctx: &PlaybackContext) -> bool {
    let session = ctx.session.borrow();
    let Some(item) = session.current().cloned() else {
        return false;
    };
    let identity = session.current_identity().cloned();
    drop(session);
    if identity
        .as_ref()
        .is_some_and(|identity| is_library_source(identity.media_key.source_id))
    {
        // Stop and supersede the prior output before beginning the async DB
        // lookup. Stop, Next, Previous, and replay all advance the generation,
        // so a late result can never reach a receiver after ownership moves.
        ctx.active_output.borrow().stop();
        let generation = ctx.session.borrow_mut().begin_pending_resolution();
        ctx.active_output.borrow().set_event_generation(generation);
        update_now_playing_ui(ctx, &item, identity.as_ref(), None);

        let track_id = identity
            .as_ref()
            .map(|identity| identity.media_key.track_id.clone())
            .expect("local queue item has an identity");
        let configured_roots = ctx.app_config.borrow().library_paths.clone();
        let (resolved_tx, resolved_rx) = async_channel::bounded(1);
        ctx.rt_handle.spawn(async move {
            let resolved = match crate::db::connection::init_db().await {
                Ok(db) => {
                    crate::local::resolver::resolve_track(&db, track_id.as_str(), &configured_roots)
                        .await
                }
                Err(source) => {
                    Err(crate::local::resolver::LocalMediaResolutionError::Database { source })
                }
            };
            let _ = resolved_tx.send(resolved).await;
        });

        let session = Rc::clone(&ctx.session);
        let active_output = Rc::clone(&ctx.active_output);
        let media_ctrl = Rc::clone(&ctx.media_ctrl);
        let app_config = Rc::clone(&ctx.app_config);
        let album_art = ctx.album_art.clone();
        glib::MainContext::default().spawn_local(async move {
            match resolved_rx.recv().await {
                Ok(Ok(media))
                    if media.matches_current_configuration(&app_config.borrow().library_paths) =>
                {
                    if session.borrow_mut().finish_pending_resolution(generation) {
                        let artwork_media = media.clone();
                        let accepted = active_output.borrow().load_local(media);
                        if !accepted {
                            let marked = session.borrow_mut().mark_load_rejected(generation);
                            debug_assert!(marked, "current local load remains retryable");
                            album_art::invalidate();
                        } else {
                            album_art::update_resolved_file_album_art(
                                &album_art,
                                artwork_media,
                            );
                        }
                    }
                }
                Ok(Ok(_)) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!("Local media root changed before output handoff");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
                Ok(Err(error)) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!(error = %error, "Could not resolve local track by its library identity");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
                Err(_) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!("Local media resolver stopped before returning a result");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
            }
        });
        return true;
    }

    if let Some(expected_session_epoch) = item.session_epoch {
        // Retire the prior output/ticket before awaiting the source resolver.
        // The Stop captures the old event generation; the new generation below
        // therefore rejects any delayed terminal event it produces.
        ctx.active_output.borrow().stop();
        let generation = ctx.session.borrow_mut().begin_pending_resolution();
        ctx.active_output.borrow().set_event_generation(generation);
        update_now_playing_ui(ctx, &item, identity.as_ref(), None);

        let identity = identity
            .as_ref()
            .expect("remote queue item has stable media identity");
        let source_id = identity.media_key.source_id;
        let track_id = identity.media_key.track_id.clone();
        let remote_sources = ctx.remote_sources.clone();
        let session = Rc::clone(&ctx.session);
        let active_output = Rc::clone(&ctx.active_output);
        let media_ctrl = Rc::clone(&ctx.media_ctrl);
        glib::MainContext::default().spawn_local(async move {
            let resolved = remote_sources
                .resolve_stream(source_id, expected_session_epoch, track_id)
                .await;
            match resolved {
                Ok(request) if request.is_active() => {
                    if session.borrow_mut().finish_pending_resolution(generation) {
                        let accepted = active_output.borrow().load_resolved(request);
                        if !accepted {
                            let marked = session.borrow_mut().mark_load_rejected(generation);
                            debug_assert!(marked, "current resolved load remains retryable");
                        }
                    }
                }
                Ok(_) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!("Could not resolve track: source lease is inactive");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
                Err(error) => {
                    if session.borrow_mut().fail_pending_resolution(generation) {
                        warn!(error = %error, "Could not resolve track through its live source session");
                        active_output.borrow().stop();
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
            }
        });
        return true;
    }

    if item.uri.is_empty() {
        warn!("Track has no playable URI");
        return false;
    }

    let playback_uri = item.uri.clone();

    // Each output resolves the source at its own trust boundary. Chromecast
    // serves local files over its embedded LAN server; authenticated remote
    // media is exchanged for an app-owned proxy ticket before Chromecast,
    // MPD, local playbin, or AirPlay uridecodebin can consume it. No output
    // swap is needed here.

    tracing::debug!("Playing track");

    let load = {
        let output = ctx.active_output.borrow();
        ctx.session
            .borrow_mut()
            .load_current_direct(output.as_ref())
    };
    let Some(load) = load else {
        debug_assert!(false, "current direct queue item is loadable");
        return false;
    };
    tracing::debug!(
        generation = ?load.generation,
        accepted = load.accepted,
        "Direct queue item handed to output"
    );
    update_now_playing_ui(ctx, &item, identity.as_ref(), Some(&playback_uri));

    true
}

fn update_now_playing_ui(
    ctx: &PlaybackContext,
    item: &QueueItem,
    identity: Option<&PlaybackIdentity>,
    direct_playback_uri: Option<&str>,
) {
    ctx.title_label.set_label(&item.title);
    ctx.title_label.set_tooltip_text(Some(&item.title));
    let artist_album = format!("{} \u{2014} {}", item.artist, item.album);
    ctx.artist_label.set_label(&artist_album);
    ctx.artist_label.set_tooltip_text(Some(&artist_album));

    // Scroll only when the queue's source and item are present in the current
    // view. Navigation still works when the user is viewing another source or
    // has filtered the playing item out.
    if identity.is_some_and(|identity| {
        identity_belongs_to_source(identity, &ctx.active_source_key.borrow())
    }) {
        if let Some(position) = find_queue_item_position(
            ctx.model.n_items(),
            identity.map_or("", |identity| identity.media_key.track_id.as_str()),
            item.occurrence,
            item.row_instance_id,
            |index| {
                ctx.model
                    .item(index)
                    .and_downcast::<TrackObject>()
                    .map(|track| (track.row_instance_id(), track.track_id()))
            },
        ) {
            ctx.column_view.scroll_to(
                position,
                None,
                gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                None,
            );
        }
    }

    // ── Update album art ─────────────────────────────────────────
    if let (Some(identity), Some(expected_session_epoch)) = (identity, item.session_epoch) {
        let generation = album_art::begin_remote_album_art(&ctx.album_art);
        let remote_sources = ctx.remote_sources.clone();
        let source_id = identity.media_key.source_id;
        let track_id = identity.media_key.track_id.clone();
        let album_art = ctx.album_art.clone();
        glib::MainContext::default().spawn_local(async move {
            match remote_sources
                .resolve_artwork(source_id, expected_session_epoch, track_id)
                .await
            {
                Ok(Some(request)) => {
                    album_art::fetch_resolved_album_art(&album_art, request, generation);
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(error = %error, "Could not resolve artwork through its live source session");
                }
            }
        });
    } else if !item.cover_art_url.is_empty() {
        album_art::fetch_remote_album_art(&ctx.album_art, &item.cover_art_url);
    } else if let Some(playback_uri) = direct_playback_uri {
        // Transitional direct file sources extract from their locator until
        // their source adapters provide retained file authority.
        album_art::update_direct_file_album_art(&ctx.album_art, playback_uri);
    } else {
        album_art::invalidate();
        ctx.album_art
            .set_icon_name(Some("audio-x-generic-symbolic"));
    }

    if let Some(ref mut ctrl) = *ctx.media_ctrl.borrow_mut() {
        ctrl.update_metadata(&item.title, &item.artist, &item.album);
        // The OS transports have no buffering state. Publish Playing when a
        // load is accepted and let a later Paused/Stopped event correct it.
        ctrl.update_playback(true);
    }
}

fn find_queue_item_position(
    item_count: u32,
    track_id: &str,
    target_occurrence: usize,
    row_instance_id: Option<u64>,
    mut item_at: impl FnMut(u32) -> Option<(u64, String)>,
) -> Option<u32> {
    let items: Vec<Option<(u64, String)>> = (0..item_count).map(&mut item_at).collect();
    if let Some(row_instance_id) = row_instance_id {
        if let Some(position) = items.iter().position(|item| {
            item.as_ref()
                .is_some_and(|(candidate, _)| *candidate == row_instance_id)
        }) {
            return u32::try_from(position).ok();
        }
    }

    let mut occurrence = 0usize;
    for (index, item) in items.into_iter().enumerate() {
        if item.as_ref().map(|(_, id)| id.as_str()) != Some(track_id) {
            continue;
        }
        if occurrence == target_occurrence {
            return u32::try_from(index).ok();
        }
        occurrence += 1;
    }
    None
}

/// Advance to the next track, respecting shuffle and repeat-all.
///
/// Returns `true` if a new track was loaded, `false` if we've reached
/// the end (caller should reset to idle).
pub fn advance_track(ctx: &PlaybackContext, repeat_mode: RepeatMode, shuffle: bool) -> bool {
    let previous = ctx.session.borrow().clone();
    if ctx
        .session
        .borrow_mut()
        .advance(repeat_mode, shuffle)
        .is_none()
    {
        return false;
    }
    if play_current(ctx) {
        true
    } else {
        *ctx.session.borrow_mut() = previous;
        false
    }
}

/// Step back to the previous track, respecting repeat-all wrap-around.
///
/// This is the positional inverse of [`advance_track`] and intentionally
/// has no "restart current track if past N seconds" behaviour — that
/// heuristic belongs to the UI/key callers, which know what threshold
/// they want to use. Returns `true` if a new track was loaded.
pub fn previous_track(ctx: &PlaybackContext, repeat_mode: RepeatMode, shuffle: bool) -> bool {
    let previous = ctx.session.borrow().clone();
    if ctx
        .session
        .borrow_mut()
        .previous(repeat_mode, shuffle)
        .is_none()
    {
        return false;
    }
    if play_current(ctx) {
        true
    } else {
        *ctx.session.borrow_mut() = previous;
        false
    }
}

/// Replay the current queue item without consulting the mutable view.
pub fn replay_current(ctx: &PlaybackContext) -> bool {
    play_current(ctx)
}

/// Play a local file directly, bypassing the library tracklist.
///
/// Used by the OS "Open With" / `xdg-open` handler.  Reads tags via
/// lofty, updates the now-playing UI (labels, album art, OS media
/// overlay), and asks the active output to play the file. The file becomes a
/// one-item external playback queue, so Next/Previous cannot jump into an
/// unrelated visible source after it ends.
///
/// Returns `true` if playback was initiated, `false` if the file could
/// not be parsed or has no playable URI representation.
pub fn play_local_file(path: &std::path::Path, ctx: &PlaybackContext) -> bool {
    let parsed = match crate::local::tag_parser::parse_audio_file(path) {
        Ok(p) => p,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Open With: failed to parse audio file");
            return false;
        }
    };

    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Ok(uri) = url::Url::from_file_path(&canonical) else {
        warn!(path = %path.display(), "Open With: path cannot be represented as a file URI");
        return false;
    };
    let item = QueueItem::external(
        uri.to_string(),
        parsed.title,
        parsed.artist_name,
        parsed.album_title,
    );
    let previous = ctx.session.borrow().clone();
    if !ctx.session.borrow_mut().replace_queue(vec![item], 0) {
        return false;
    }
    if !play_current(ctx) {
        *ctx.session.borrow_mut() = previous;
        return false;
    }

    tracing::info!(path = %path.display(), "Open With: playback started");
    true
}

/// Format milliseconds as `m:ss` (or `h:mm:ss` for ≥ 1 hour).
pub fn format_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingOutputState {
        generations: Vec<PlayerEventGeneration>,
        loads: Vec<String>,
        stops: usize,
    }

    struct RecordingOutput {
        state: Rc<RefCell<RecordingOutputState>>,
        reject_loads: Cell<usize>,
        volume: f64,
    }

    impl RecordingOutput {
        fn new(reject_loads: usize) -> (Self, Rc<RefCell<RecordingOutputState>>) {
            let state = Rc::new(RefCell::new(RecordingOutputState::default()));
            (
                Self {
                    state: Rc::clone(&state),
                    reject_loads: Cell::new(reject_loads),
                    volume: 0.5,
                },
                state,
            )
        }
    }

    impl AudioOutput for RecordingOutput {
        fn name(&self) -> &str {
            "recording"
        }

        fn output_type(&self) -> crate::audio::output::OutputType {
            crate::audio::output::OutputType::Local
        }

        fn supports_volume(&self) -> bool {
            true
        }

        fn load_uri(&self, uri: &str) -> bool {
            self.state.borrow_mut().loads.push(uri.to_string());
            let remaining = self.reject_loads.get();
            if remaining == 0 {
                true
            } else {
                self.reject_loads.set(remaining - 1);
                false
            }
        }

        fn load_resolved(&self, _request: crate::architecture::media::ResolvedHttpRequest) -> bool {
            false
        }

        fn load_local(&self, _media: crate::local::resolver::ResolvedLocalMedia) -> bool {
            false
        }

        fn set_event_generation(&self, generation: PlayerEventGeneration) {
            self.state.borrow_mut().generations.push(generation);
        }

        fn play(&self) {}

        fn pause(&self) {}

        fn stop(&self) {
            self.state.borrow_mut().stops += 1;
        }

        fn toggle_play_pause(&self) {}

        fn seek_to(&self, _position_ms: u64) {}

        fn set_volume(&mut self, level: f64) {
            self.volume = level;
        }

        fn volume(&self) -> f64 {
            self.volume
        }

        fn state(&self) -> crate::audio::PlayerState {
            crate::audio::PlayerState::Stopped
        }

        fn position_ms(&self) -> Option<u64> {
            None
        }
    }

    fn item(source: &str, id: &str) -> QueueItem {
        let view = queue_view(source).expect("test source identity");
        let identity = PlaybackIdentity::new(
            &view,
            TrackId::new(id.to_string()).expect("test track identity"),
        );
        QueueItem {
            identity,
            occurrence: 0,
            row_instance_id: None,
            session_epoch: None,
            uri: if is_library_source(view.source_id) {
                String::new()
            } else {
                format!("https://media.invalid/{id}")
            },
            title: id.to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            cover_art_url: String::new(),
        }
    }

    fn protected_item(source: &str, id: &str) -> QueueItem {
        let mut item = item(source, id);
        item.uri.clear();
        item.session_epoch = Some(7);
        item
    }

    fn projected_row(id: &str, uri: &str) -> TrackObject {
        let row = TrackObject::new(
            1, "Title", 60, "Artist", "Album", "", "", 0, "", 0, 0, 0, "", uri,
        );
        row.set_track_id(id);
        row
    }

    fn playback_row(id: &str) -> TrackObject {
        projected_row(id, &format!("file:///music/{id}.flac"))
    }

    fn ids(session: &PlaybackSession) -> Vec<String> {
        session
            .queue
            .iter()
            .map(|entry| entry.identity.media_key.track_id.as_str().to_string())
            .collect()
    }

    fn library_ids(session: &PlaybackSession) -> HashSet<&str> {
        session
            .library_track_ids()
            .into_iter()
            .map(TrackId::as_str)
            .collect()
    }

    fn current_id(session: &PlaybackSession) -> &str {
        session
            .current_identity()
            .expect("current identity")
            .media_key
            .track_id
            .as_str()
    }

    fn current_source(session: &PlaybackSession) -> SourceId {
        session
            .current_identity()
            .expect("current identity")
            .media_key
            .source_id
    }

    fn item_from_row(source: &str, row: &TrackObject, occurrence: usize) -> QueueItem {
        let view = queue_view(source).expect("test source identity");
        let identity = PlaybackIdentity::new(
            &view,
            TrackId::new(row.track_id()).expect("test track identity"),
        );
        QueueItem::from_track(identity, row, occurrence)
    }

    fn refreshed_metadata() -> QueueTrackRefresh {
        QueueTrackRefresh {
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            cover_art_url: String::new(),
        }
    }

    fn refresh(session: &mut PlaybackSession, track_id: &str, update: QueueTrackRefresh) -> usize {
        session.refresh_library_tracks(&HashMap::from([(
            TrackId::new(track_id.to_string()).expect("test track identity"),
            update,
        )]))
    }

    #[test]
    fn a_library_metadata_refresh_preserves_pathless_queue_identity_and_shuffle() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            0,
        ));
        // Enter shuffle so the cycle's index bookkeeping is live.
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let cursor = session.current_index;
        let shuffle = session.shuffle.clone().expect("shuffle state exists");

        assert_eq!(refresh(&mut session, "b", refreshed_metadata()), 1);

        assert!(session.queue.iter().all(|item| item.uri.is_empty()));
        assert_eq!(session.queue[1].title, "Title");
        assert_eq!(
            ids(&session),
            ["a", "b", "c"],
            "identity, order, and length are the coordinates the cursor indexes into"
        );
        assert_eq!(session.current_index, cursor);
        assert_eq!(
            session.shuffle.as_ref().map(|state| &state.remaining),
            Some(&shuffle.remaining)
        );
    }

    #[test]
    fn a_metadata_refresh_reaches_the_item_playing_right_now_without_a_locator() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a"), item("local", "b")], 1));

        assert_eq!(refresh(&mut session, "b", refreshed_metadata()), 1);

        let current = session.current().expect("current item");
        assert_eq!(current.title, "Title");
        assert!(current.uri().is_empty());
    }

    #[test]
    fn a_playlist_queue_follows_the_same_library_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("playlist:favourites", "a"), item("local", "a")],
            0,
        ));
        assert_eq!(
            library_ids(&session),
            HashSet::from(["a"]),
            "duplicate playlist/local occurrences need one snapshot lookup"
        );

        assert_eq!(
            refresh(&mut session, "a", refreshed_metadata()),
            2,
            "a playlist is a projection of the library, so it holds library track IDs"
        );
    }

    #[test]
    fn a_library_refresh_never_reinterprets_another_source_s_track_id() {
        let mut session = PlaybackSession::default();
        let external = QueueItem::external(
            "file:///downloads/a.flac".to_string(),
            "External".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        assert!(session.replace_queue(
            vec![
                external,
                item("https://subsonic.invalid", "a"),
                item("local", "a")
            ],
            0,
        ));
        assert_eq!(
            library_ids(&session),
            HashSet::from(["a"]),
            "only local-library sources participate in snapshot filtering"
        );

        // "a" is a library UUID here, but a remote backend's native ID — and an
        // external file's URI — are namespaced by their own source.
        assert_eq!(refresh(&mut session, "a", refreshed_metadata()), 1);
        assert_eq!(session.queue[0].uri, "file:///downloads/a.flac");
        assert_eq!(session.queue[1].uri, "https://media.invalid/a");
        assert!(session.queue[2].uri.is_empty());
    }

    #[test]
    fn local_and_playlist_queue_capture_discards_the_row_locator() {
        let local = projected_row("legacy:local-id", "file:///music/captured.flac");
        let playlist = projected_row("legacy:local-id", "file:///music/captured.flac");
        let remote = projected_row("remote-id", "https://media.invalid/stream");

        let local_item = item_from_row("local", &local, 0);
        let playlist_item = item_from_row("playlist:favourites", &playlist, 0);
        let remote_item = item_from_row("https://server.invalid", &remote, 0);

        assert_eq!(
            local_item.identity.media_key.track_id.as_str(),
            "legacy:local-id"
        );
        assert_eq!(
            playlist_item.identity.media_key.track_id.as_str(),
            "legacy:local-id"
        );
        assert_eq!(
            local_item.identity.media_key,
            playlist_item.identity.media_key
        );
        assert_eq!(
            playlist_item.identity.view_origin,
            Some(ViewOrigin::Playlist("favourites".to_string()))
        );
        assert!(local_item.uri.is_empty());
        assert!(playlist_item.uri.is_empty());
        assert_eq!(remote_item.uri, "https://media.invalid/stream");
    }

    #[test]
    fn authenticated_remote_queue_capture_preserves_epoch_and_stays_pathless() {
        let remote = projected_row("native/remote.id", "");
        remote.set_remote_session_epoch(77);

        let captured = item_from_row("https://server.invalid", &remote, 0);

        assert_eq!(
            captured.identity.media_key.track_id.as_str(),
            "native/remote.id"
        );
        assert_eq!(captured.session_epoch(), Some(77));
        assert!(captured.uri().is_empty());
        assert!(captured.cover_art_url.is_empty());

        let direct = item_from_row(
            "https://another.invalid",
            &projected_row("direct", "https://media.invalid/direct"),
            0,
        );
        assert_eq!(direct.session_epoch(), None);
        assert_eq!(direct.uri(), "https://media.invalid/direct");
    }

    #[test]
    fn authenticated_remote_item_cannot_bypass_resolution_with_a_cached_uri() {
        let mut session = PlaybackSession::default();
        let mut remote = protected_item("https://server.invalid", "native/remote.id");
        remote.uri = "https://media.invalid/should-not-load".to_string();
        assert!(session.replace_queue(vec![remote], 0));

        let (output, output_state) = RecordingOutput::new(0);
        assert!(session.load_current_direct(&output).is_none());
        assert!(output_state.borrow().loads.is_empty());
        assert!(output_state.borrow().generations.is_empty());
    }

    #[test]
    fn projected_playlist_rows_follow_committed_uris_without_losing_occurrences() {
        let first = projected_row("a", "file:///music/old-a.flac");
        let unrelated = projected_row("b", "file:///music/b.flac");
        let duplicate = projected_row("a", "file:///music/old-a.flac");
        let rows = vec![first, unrelated, duplicate];
        let identities: Vec<u64> = rows.iter().map(TrackObject::row_instance_id).collect();

        let renamed = projected_row("a", "file:///music/renamed-a.flac");
        let empty = projected_row("b", "");
        assert_eq!(refresh_projected_library_uris(&rows, &[renamed, empty]), 2);

        assert_eq!(rows[0].uri(), "file:///music/renamed-a.flac");
        assert_eq!(rows[1].uri(), "file:///music/b.flac");
        assert_eq!(rows[2].uri(), "file:///music/renamed-a.flac");
        assert_eq!(
            rows.iter()
                .map(TrackObject::row_instance_id)
                .collect::<Vec<_>>(),
            identities,
            "URI refresh must preserve duplicate occurrence identity and order"
        );
    }

    #[test]
    fn sorting_the_view_does_not_reorder_the_playback_snapshot() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            1,
        ));

        // This represents the independent GTK view after a descending sort.
        let sorted_view = ["c", "b", "a"];
        assert_eq!(sorted_view[0], "c");
        assert_eq!(ids(&session), ["a", "b", "c"]);

        assert_eq!(session.advance(RepeatMode::Off, false), Some(2));
        assert_eq!(current_id(&session), "c");
    }

    #[test]
    fn production_snapshot_survives_sort_filter_navigation_and_owns_output_events() {
        let source_key = "fixture-device-a";
        let store = gtk::gio::ListStore::new::<TrackObject>();
        for id in ["a", "b", "c"] {
            store.append(&playback_row(id));
        }
        let captured = capture_visible_queue(&store, source_key, 1).expect("visible B is captured");
        assert_eq!(
            captured
                .items
                .iter()
                .map(|entry| entry.identity.media_key.track_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(captured.items, captured.selected_index));
        let (output, output_state) = RecordingOutput::new(0);
        let first = session
            .load_current_direct(&output)
            .expect("captured B reaches the output");
        assert!(first.accepted);
        let stale_b_event = crate::audio::PlayerEvent::ended(first.generation);
        assert_eq!(
            current_source(&session),
            SourceId::removable(source_key).expect("fixture source ID")
        );
        assert_eq!(current_id(&session), "b");

        // Exercise the same ListModel projection boundary consumed from
        // production's SortListModel, without constructing a display-bound
        // GTK widget/model in headless CI.
        store.remove_all();
        for id in ["c", "b", "a"] {
            store.append(&playback_row(id));
        }
        assert_eq!(
            (0..store.n_items())
                .map(|index| {
                    store
                        .item(index)
                        .and_downcast::<TrackObject>()
                        .expect("sorted TrackObject")
                        .track_id()
                })
                .collect::<Vec<_>>(),
            ["c", "b", "a"]
        );

        // Browser filtering rebuilds the underlying projection. Remove B and
        // prove Next still follows the playback-owned A/B/C snapshot.
        store.remove(1);
        assert_eq!(store.n_items(), 2);
        assert_eq!(session.advance(RepeatMode::Off, false), Some(2));
        let c_load = session
            .load_current_direct(&output)
            .expect("filtered-out queue neighbor does not retarget Next");
        assert!(c_load.accepted);
        assert_eq!(current_id(&session), "c");
        assert!(!session.accepts_event_generation(stale_b_event.generation()));
        assert!(session.accepts_event_generation(c_load.generation));

        // Sidebar navigation replaces the projection and source key, but does
        // not install a queue until the user explicitly starts one there.
        store.remove_all();
        store.append(&playback_row("remote-x"));
        let remote_projection =
            capture_visible_queue(&store, "remote-server", 0).expect("remote view captures");
        assert_eq!(
            remote_projection.items[0].identity.media_key.source_id,
            SourceId::removable("remote-server").expect("replacement source ID")
        );
        assert_eq!(session.previous(RepeatMode::Off, false), Some(1));
        let b_again = session
            .load_current_direct(&output)
            .expect("Previous loads B from the local snapshot, not remote view");
        assert_eq!(
            current_source(&session),
            SourceId::removable(source_key).expect("fixture source ID")
        );
        assert_eq!(current_id(&session), "b");
        assert!(session.accepts_event_generation(b_again.generation));
        assert_eq!(
            output_state.borrow().loads,
            [
                "file:///music/b.flac",
                "file:///music/c.flac",
                "file:///music/b.flac"
            ]
        );

        stop_owned_playback(&mut session, &output);
        assert!(!session.has_current());
        assert!(!session.accepts_event_generation(b_again.generation));
        assert_eq!(output_state.borrow().stops, 1);
    }

    #[test]
    fn filtering_the_view_does_not_remove_items_from_the_playback_snapshot() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            0,
        ));

        // The playing queue still contains B even if the current view does not.
        let filtered_view = ["a", "c"];
        assert!(!filtered_view.contains(&"b"));
        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        assert_eq!(current_id(&session), "b");
    }

    #[test]
    fn source_navigation_does_not_change_playing_source_or_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "track-a")], 0));

        let active_view_source = "remote-server";
        assert_ne!(
            current_source(&session),
            SourceId::removable(active_view_source).expect("source ID")
        );
        assert_eq!(current_source(&session), SourceId::local());
        assert_eq!(current_id(&session), "track-a");
    }

    #[test]
    fn sequential_previous_and_repeat_all_use_snapshot_boundaries() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            2,
        ));

        assert_eq!(session.advance(RepeatMode::Off, false), None);
        assert_eq!(current_id(&session), "c");
        assert_eq!(session.advance(RepeatMode::All, false), Some(0));
        assert_eq!(current_id(&session), "a");
        assert_eq!(session.previous(RepeatMode::All, false), Some(2));
        assert_eq!(current_id(&session), "c");
    }

    #[test]
    fn eos_repeat_policies_are_bound_to_the_snapshot_cursor() {
        let queue = vec![item("source", "a"), item("source", "b")];

        let mut repeat_one = PlaybackSession::default();
        assert!(repeat_one.replace_queue(queue.clone(), 1));
        // EOS repeat-one calls replay_current and does not move the cursor.
        assert_eq!(current_id(&repeat_one), "b");

        let mut repeat_off = PlaybackSession::default();
        assert!(repeat_off.replace_queue(queue.clone(), 1));
        assert_eq!(repeat_off.advance(RepeatMode::Off, false), None);

        let mut repeat_all = PlaybackSession::default();
        assert!(repeat_all.replace_queue(queue, 1));
        assert_eq!(repeat_all.advance(RepeatMode::All, false), Some(0));
        assert_eq!(current_id(&repeat_all), "a");
    }

    #[test]
    fn shuffle_visits_each_snapshot_item_once_before_repeat() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            0,
        ));

        let mut visited = HashSet::from(["a".to_string()]);
        for _ in 0..2 {
            assert!(session.advance(RepeatMode::Off, true).is_some());
            visited.insert(current_id(&session).to_string());
        }
        assert_eq!(visited, HashSet::from(["a".into(), "b".into(), "c".into()]));
        assert_eq!(session.advance(RepeatMode::Off, true), None);

        // Repeat-all starts another shuffle cycle instead of ending.
        assert!(session.advance(RepeatMode::All, true).is_some());
    }

    #[test]
    fn shuffled_previous_follows_play_history() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![
                item("source", "a"),
                item("source", "b"),
                item("source", "c")
            ],
            0,
        ));
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let previous_id = current_id(&session).to_string();
        assert!(session.advance(RepeatMode::Off, true).is_some());
        assert!(session.previous(RepeatMode::Off, true).is_some());
        assert_eq!(current_id(&session), previous_id);
    }

    #[test]
    fn external_file_is_a_one_item_queue() {
        let mut session = PlaybackSession::default();
        let external = QueueItem::external(
            "file:///tmp/example.flac".to_string(),
            "Example".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        let first_source = external.identity.media_key.source_id;
        let first_track = external.identity.media_key.track_id.clone();
        let another = QueueItem::external(
            "file:///tmp/example.flac".to_string(),
            "Example".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        assert_ne!(first_source, another.identity.media_key.source_id);
        assert_ne!(first_track, another.identity.media_key.track_id);
        assert_ne!(first_track.as_str(), first_source.to_string());
        assert!(session.replace_queue(vec![external], 0));
        assert_eq!(current_source(&session), first_source);
        assert_ne!(current_source(&session), SourceId::local());
        assert_eq!(session.advance(RepeatMode::Off, false), None);
        assert_eq!(session.advance(RepeatMode::All, false), Some(0));
    }

    #[test]
    fn clearing_a_session_removes_queue_identity() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a")], 0));
        session.clear();
        assert!(!session.has_current());
        assert!(session.queue.is_empty());
    }

    #[test]
    fn source_retirement_clears_only_its_own_queue() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![protected_item("remote-a", "a")], 0));
        let retired_generation = session.begin_pending_resolution();

        assert!(!session.clear_if_source("remote-b"));
        assert!(session.accepts_event_generation(retired_generation));
        assert!(session.has_current());

        assert!(session.clear_if_source("remote-a"));
        assert!(!session.accepts_event_generation(retired_generation));
        assert!(!session.has_current());
        assert!(session.queue.is_empty());
    }

    #[test]
    fn playlist_view_origin_is_separate_from_local_media_identity() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("playlist:favourites", "local-track")], 0));
        let generation = session.begin_pending_resolution();
        let identity = session.current_identity().expect("identity");
        assert_eq!(identity.media_key.source_id, SourceId::local());
        assert_eq!(
            identity.view_origin,
            Some(ViewOrigin::Playlist("favourites".to_string()))
        );

        assert!(!session.clear_if_source("playlist:other"));
        assert!(!session.clear_if_source("playlist:favourites"));
        assert!(session.accepts_event_generation(generation));
        assert!(session.clear_if_source(LOCAL_SOURCE_KEY));
        assert!(!session.has_current());

        assert!(session.replace_queue(vec![item(LOCAL_SOURCE_KEY, "local-track")], 0));
        assert!(!session.clear_if_source("playlist:favourites"));
        assert!(session.has_current());
    }

    #[test]
    fn radio_queries_share_media_namespace_but_not_view_ownership() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("radio-topvote", "station-uuid")], 0));
        let identity = session.current_identity().expect("identity");
        assert_eq!(identity.media_key.source_id, SourceId::radio_browser());
        assert_eq!(
            identity.view_origin,
            Some(ViewOrigin::Radio("radio-topvote".to_string()))
        );
        assert!(!session.clear_if_source("radio-nearme"));
        assert!(!session.clear_if_source("radio-topvote"));
        assert!(session.clear_if_source(&SourceId::radio_browser().to_string()));
    }

    #[test]
    fn removable_queue_identity_is_namespaced_by_logical_device() {
        let device = "device:uuid:01234567-89ab-cdef-0123-456789abcdef";
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item(device, "unix:4172746973742f547261636b2e666c6163")],
            0
        ));
        let identity = session.current_identity().expect("identity");
        assert_eq!(
            identity.media_key.source_id,
            SourceId::removable(device).expect("device source ID")
        );
        assert_eq!(
            identity.media_key.track_id.as_str(),
            "unix:4172746973742f547261636b2e666c6163"
        );
    }

    #[test]
    fn stale_player_events_are_rejected_after_track_change_and_reset() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a"), item("local", "b")], 0));
        let first_generation = session.begin_event_generation();
        let stale_eos = crate::audio::PlayerEvent::ended(first_generation);
        assert!(session.accepts_event_generation(stale_eos.generation()));

        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        let second_generation = session.begin_event_generation();
        assert!(!session.accepts_event_generation(stale_eos.generation()));
        assert!(session.accepts_event_generation(second_generation));

        session.clear();
        assert!(!session.accepts_event_generation(second_generation));
    }

    #[test]
    fn protected_resolution_is_owned_by_exact_playback_generation() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("remote", "a"), item("remote", "b")], 0));

        let stale = session.begin_pending_resolution();
        assert!(session.is_resolution_pending());
        assert_eq!(session.advance(RepeatMode::Off, false), Some(1));
        let current = session.begin_pending_resolution();

        assert!(!session.finish_pending_resolution(stale));
        assert!(session.finish_pending_resolution(current));
        assert!(!session.is_resolution_pending());
        assert!(!session.resolution_failed);
    }

    #[test]
    fn failed_resolution_is_retryable_and_stop_invalidates_it() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("remote", "a")], 0));
        let failed = session.begin_pending_resolution();

        assert!(session.fail_pending_resolution(failed));
        assert!(session.resolution_failed);
        assert!(!session.is_resolution_pending());

        let retry = session.begin_pending_resolution();
        assert!(!session.resolution_failed);
        session.clear();
        assert!(!session.finish_pending_resolution(retry));
        assert!(!session.has_current());
    }

    #[test]
    fn pending_pause_cancels_completion_and_keeps_play_retryable() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![protected_item("remote", "a")], 0));
        let cancelled = session.begin_pending_resolution();

        assert!(session.cancel_pending_resolution_for_retry());
        assert!(!session.is_resolution_pending());
        assert!(session.resolution_failed);
        assert!(session.has_current());
        assert!(!session.finish_pending_resolution(cancelled));

        let retry = session.begin_pending_resolution();
        assert_ne!(retry, cancelled);
        assert!(session.finish_pending_resolution(retry));
        assert!(!session.resolution_failed);
    }

    #[test]
    fn accepted_resolved_output_error_retries_identity_but_plain_direct_error_does_not() {
        let mut protected = PlaybackSession::default();
        assert!(protected.replace_queue(vec![protected_item("remote", "a")], 0));
        let failed_load = protected.begin_pending_resolution();
        assert!(protected.finish_pending_resolution(failed_load));

        assert!(protected.mark_resolved_load_failed(failed_load));
        assert!(protected.resolution_failed);
        assert!(!protected.accepts_event_generation(failed_load));
        let retry = protected.begin_pending_resolution();
        assert_ne!(retry, failed_load);

        let mut daap = PlaybackSession::default();
        let mut daap_item = item("daap-source", "a");
        daap_item.uri.clear();
        daap_item.session_epoch = Some(9);
        assert!(daap.replace_queue(vec![daap_item], 0));
        let daap_load = daap.begin_event_generation();
        assert!(daap.mark_resolved_load_failed(daap_load));
        assert!(daap.resolution_failed);

        let mut local = PlaybackSession::default();
        assert!(local.replace_queue(vec![item("local", "a")], 0));
        let local_load = local.begin_pending_resolution();
        assert!(local.finish_pending_resolution(local_load));
        assert!(local.mark_resolved_load_failed(local_load));
        assert!(local.resolution_failed);

        let mut direct = PlaybackSession::default();
        assert!(direct.replace_queue(vec![item("radio", "a")], 0));
        let direct_load = direct.begin_event_generation();
        assert!(!direct.mark_resolved_load_failed(direct_load));
        assert!(!direct.resolution_failed);
        assert!(direct.accepts_event_generation(direct_load));
    }

    #[test]
    fn synchronous_load_rejection_keeps_exact_direct_generation_retryable() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a")], 0));
        let rejected = session.begin_event_generation();

        assert!(session.mark_load_rejected(rejected));
        assert!(session.resolution_failed);
        assert!(session.accepts_event_generation(rejected));
        assert!(
            !session.mark_resolved_load_failed(rejected),
            "synchronous refusal has no output state to stop"
        );

        let retry = session.begin_event_generation();
        assert_ne!(retry, rejected);
        assert!(!session.resolution_failed);
        assert!(!session.mark_load_rejected(rejected));
        assert!(session.mark_load_rejected(retry));
    }

    #[test]
    fn buffering_reset_invalidates_timer_and_state() {
        let tracker = BufferingTracker::default();
        let stale_timer = tracker.begin();
        assert!(tracker.is_current(stale_timer));
        assert!(tracker.is_buffering());

        tracker.invalidate();

        assert!(!tracker.is_current(stale_timer));
        assert!(!tracker.is_buffering());
    }

    #[test]
    fn os_stop_then_play_starts_a_fresh_visible_queue() {
        let session = RefCell::new(PlaybackSession::default());
        assert!(session
            .borrow_mut()
            .replace_queue(vec![item("local", "a")], 0));
        assert_eq!(
            resolve_session_play_request(&session, 3, false),
            PlayRequest::Resume
        );

        session.borrow_mut().clear();

        assert_eq!(
            resolve_session_play_request(&session, 3, false),
            PlayRequest::StartAt(0)
        );
        assert!(
            session
                .try_borrow_mut()
                .expect("play-request resolution must release its immutable borrow")
                .replace_queue(vec![item("remote", "fresh")], 0),
            "the StartAt arm must be able to install a fresh queue"
        );
    }

    #[test]
    fn duplicate_track_occurrences_keep_stable_identity_but_select_distinct_rows() {
        let mut first = item("playlist:one", "same-track");
        first.row_instance_id = Some(11);
        let mut second = first.clone();
        second.occurrence = 1;
        second.row_instance_id = Some(22);
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![first, second], 1));

        assert_eq!(current_id(&session), "same-track");
        assert_eq!(session.current().map(|item| item.occurrence), Some(1));
        assert_eq!(
            find_queue_item_position(4, "same-track", 1, Some(22), |index| {
                [
                    (11, "same-track"),
                    (12, "other"),
                    (22, "same-track"),
                    (33, "same-track"),
                ]
                .get(index as usize)
                .map(|(row_id, track_id)| (*row_id, (*track_id).to_string()))
            }),
            Some(2)
        );
    }
}
