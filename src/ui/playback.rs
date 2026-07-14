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
use tracing::warn;

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
/// Remote backends key tracks by their own native IDs, and external files and
/// USB items fall back to their URI as an ID ([`TrackObject::track_id`]). A
/// library update must never reinterpret one of those as one of its own.
fn is_library_source(source_id: &str) -> bool {
    source_id == LOCAL_SOURCE_KEY || source_id.starts_with(PLAYLIST_SOURCE_PREFIX)
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

/// Stable identity of a track inside the source that supplied its queue.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackIdentity {
    pub source_id: String,
    pub track_id: String,
}

/// A committed library change, addressed to the queue by stable track ID.
///
/// A rename moves a track's file without changing what it is, so the queue must
/// re-resolve where to play it from while keeping the identity, position, and
/// history it captured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTrackRefresh {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_art_url: String,
}

impl QueueTrackRefresh {
    pub fn from_track(track: &TrackObject) -> Self {
        Self {
            uri: track.uri(),
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
    uri: String,
    title: String,
    artist: String,
    album: String,
    cover_art_url: String,
}

impl QueueItem {
    fn from_track(source_id: &str, track: &TrackObject, occurrence: usize) -> Self {
        Self {
            identity: PlaybackIdentity {
                source_id: source_id.to_string(),
                track_id: track.track_id(),
            },
            occurrence,
            row_instance_id: Some(track.row_instance_id()),
            uri: track.uri(),
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            cover_art_url: track.cover_art_url(),
        }
    }

    pub(crate) fn external(uri: String, title: String, artist: String, album: String) -> Self {
        Self {
            identity: PlaybackIdentity {
                source_id: "external".to_string(),
                track_id: uri.clone(),
            },
            occurrence: 0,
            row_instance_id: None,
            uri,
            title,
            artist,
            album,
            cover_art_url: String::new(),
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
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
}

impl PlaybackSession {
    pub(crate) fn replace_queue(&mut self, queue: Vec<QueueItem>, start_index: usize) -> bool {
        if queue.get(start_index).is_none() {
            return false;
        }
        self.queue = queue;
        self.current_index = Some(start_index);
        self.shuffle = None;
        true
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.current_index = None;
        self.shuffle = None;
        self.event_generation = self.event_generation.next();
    }

    /// Stable local-library IDs currently retained by the queue.
    ///
    /// A full library snapshot can be very large. Publishing this small set
    /// lets the GTK receiver avoid cloning refresh metadata for tracks the
    /// queue does not own, while preserving source namespacing.
    pub(crate) fn library_track_ids(&self) -> HashSet<&str> {
        self.queue
            .iter()
            .filter(|item| is_library_source(&item.identity.source_id))
            .map(|item| item.identity.track_id.as_str())
            .collect()
    }

    /// Re-resolve queued library items whose track the library just committed a
    /// change to, and return how many items moved.
    ///
    /// A rename preserves a track's identity but not its path, so a queue that
    /// captured the old URI would hand a dead path to the output the next time
    /// it loaded that item — on Next, on Previous, and at end of stream, where
    /// repeat-one replays the current item from the queue rather than the view.
    /// The item playing right now is refreshed too. The current output is not
    /// retargeted here, but a subsequent load or replay resolves the path where
    /// the track now lives.
    ///
    /// Items are rewritten in place. Queue length, order, and the cursor are the
    /// coordinate system that `current_index` and the shuffle history index
    /// into, so identity — not position — is what an update may address.
    pub(crate) fn refresh_library_tracks(
        &mut self,
        updates: &HashMap<String, QueueTrackRefresh>,
    ) -> usize {
        if updates.is_empty() {
            return 0;
        }

        let mut refreshed = 0;
        for item in &mut self.queue {
            if !is_library_source(&item.identity.source_id) {
                continue;
            }
            let Some(update) = updates.get(&item.identity.track_id) else {
                continue;
            };
            // A track with no playable URI is unplayable (see `play_current`).
            // Keep the captured reference rather than strand the queue item.
            if update.uri.is_empty() {
                continue;
            }
            if item.uri == update.uri
                && item.title == update.title
                && item.artist == update.artist
                && item.album == update.album
                && item.cover_art_url == update.cover_art_url
            {
                continue;
            }

            item.uri = update.uri.clone();
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
        self.event_generation
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
                return Some(current);
            }
        }

        let selected = state.remaining.pop()?;
        state.history.push(selected);
        self.current_index = Some(selected);
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
        Some(selected)
    }
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

/// Try to play the track at `position` in the given model.
///
/// Captures the visible sorted model as an immutable playback queue, then
/// starts the selected item. Later view mutations do not alter that queue.
pub fn play_track_at(position: u32, ctx: &PlaybackContext) -> bool {
    let source_id = ctx.active_source_key.borrow().clone();
    let mut selected_index = None;
    let mut queue = Vec::with_capacity(ctx.model.n_items() as usize);
    let mut occurrences: HashMap<PlaybackIdentity, usize> = HashMap::new();

    for model_index in 0..ctx.model.n_items() {
        let Some(track) = ctx.model.item(model_index).and_downcast::<TrackObject>() else {
            continue;
        };
        if model_index == position {
            selected_index = Some(queue.len());
        }
        let identity = PlaybackIdentity {
            source_id: source_id.clone(),
            track_id: track.track_id(),
        };
        let occurrence = occurrences.entry(identity).or_default();
        queue.push(QueueItem::from_track(&source_id, &track, *occurrence));
        *occurrence += 1;
    }

    let Some(selected_index) = selected_index else {
        return false;
    };
    if queue[selected_index].uri.is_empty() {
        warn!("Track has no playable URI");
        return false;
    }

    let previous = ctx.session.borrow().clone();
    if !ctx
        .session
        .borrow_mut()
        .replace_queue(queue, selected_index)
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
    match resolve_play_request(
        ctx.session.borrow().has_current(),
        ctx.model.n_items(),
        shuffle,
    ) {
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
    if ctx.session.borrow().has_current() {
        ctx.active_output.borrow().toggle_play_pause();
        true
    } else {
        play_or_start(ctx, shuffle)
    }
}

/// Invalidate the session before stopping the output so synchronously emitted
/// Stopped events are already stale. The caller owns the widget reset.
pub fn stop_playback(ctx: &PlaybackContext) {
    ctx.session.borrow_mut().clear();
    ctx.active_output.borrow().stop();
}

/// Load the current immutable queue item and refresh now-playing UI.
fn play_current(ctx: &PlaybackContext) -> bool {
    let session = ctx.session.borrow();
    let Some(item) = session.current().cloned() else {
        return false;
    };
    let identity = session.current_identity().cloned();
    drop(session);
    if item.uri.is_empty() {
        warn!("Track has no playable URI");
        return false;
    }
    let playback_uri = match resolve_live_media_reference(item.uri()) {
        Ok(uri) => uri,
        Err(error) => {
            warn!(error = %error, "Could not resolve track through its live source session");
            return false;
        }
    };

    // Local (`file://`) library tracks are cast to the device by the Chromecast
    // output itself, which serves them over its embedded LAN `MediaProxy` (see
    // `ChromecastOutput::resolve_uri`); local and AirPlay outputs play them
    // directly. Receiver-fetched outputs (Chromecast, MPD) also route
    // credential-bearing remote streams through that proxy, so the credential
    // never reaches the device. No output swap is needed here.

    tracing::debug!("Playing track");

    let generation = ctx.session.borrow_mut().begin_event_generation();
    ctx.active_output.borrow().set_event_generation(generation);
    ctx.active_output.borrow().load_uri(&playback_uri);
    ctx.title_label.set_label(&item.title);
    ctx.title_label.set_tooltip_text(Some(&item.title));
    let artist_album = format!("{} \u{2014} {}", item.artist, item.album);
    ctx.artist_label.set_label(&artist_album);
    ctx.artist_label.set_tooltip_text(Some(&artist_album));

    // Scroll only when the queue's source and item are present in the current
    // view. Navigation still works when the user is viewing another source or
    // has filtered the playing item out.
    if identity
        .as_ref()
        .is_some_and(|identity| *ctx.active_source_key.borrow() == identity.source_id)
    {
        if let Some(position) = find_queue_item_position(
            ctx.model.n_items(),
            identity.as_ref().map_or("", |identity| &identity.track_id),
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
    if !item.cover_art_url.is_empty() {
        // Remote track with a cover art URL — resolve session-scoped
        // references immediately before fetching.
        match resolve_live_media_reference(&item.cover_art_url) {
            Ok(cover_art_url) => {
                album_art::fetch_remote_album_art(&ctx.album_art, &cover_art_url);
            }
            Err(error) => {
                warn!(error = %error, "Could not resolve artwork through its live source session");
                album_art::invalidate();
                ctx.album_art
                    .set_icon_name(Some("audio-x-generic-symbolic"));
            }
        }
    } else {
        // Local track — extract from embedded tags.
        album_art::update_album_art(&ctx.album_art, &playback_uri);
    }

    if let Some(ref mut ctrl) = *ctx.media_ctrl.borrow_mut() {
        ctrl.update_metadata(&item.title, &item.artist, &item.album);
        // The OS transports have no buffering state. Publish Playing when a
        // load is accepted and let a later Paused/Stopped event correct it.
        ctrl.update_playback(true);
    }

    true
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

/// Resolve credential-free source references while passing ordinary media
/// URLs through unchanged. DAAP resolution only reads retained in-memory
/// session state and constructs a URL; it performs no network I/O.
fn resolve_live_media_reference(reference: &str) -> Result<String, String> {
    if !reference.starts_with("daap:") {
        return Ok(reference.to_string());
    }

    let reference = url::Url::parse(reference)
        .map_err(|error| format!("Invalid DAAP media reference: {error}"))?;
    crate::daap::resolve_media_url(&reference)
        .map_err(|error| error.to_string())?
        .map(|url| url.to_string())
        .ok_or_else(|| "DAAP media reference was not recognized".to_string())
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
    use std::collections::HashSet;

    use super::*;

    fn item(source: &str, id: &str) -> QueueItem {
        QueueItem {
            identity: PlaybackIdentity {
                source_id: source.to_string(),
                track_id: id.to_string(),
            },
            occurrence: 0,
            row_instance_id: None,
            uri: format!("https://media.invalid/{id}"),
            title: id.to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            cover_art_url: String::new(),
        }
    }

    fn projected_row(id: &str, uri: &str) -> TrackObject {
        let row = TrackObject::new(
            1, "Title", 60, "Artist", "Album", "", 0, "", 0, 0, 0, "", uri,
        );
        row.set_track_id(id);
        row
    }

    fn ids(session: &PlaybackSession) -> Vec<String> {
        session
            .queue
            .iter()
            .map(|entry| entry.identity.track_id.clone())
            .collect()
    }

    fn renamed(uri: &str) -> QueueTrackRefresh {
        QueueTrackRefresh {
            uri: uri.to_string(),
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            cover_art_url: String::new(),
        }
    }

    fn refresh(session: &mut PlaybackSession, track_id: &str, update: QueueTrackRefresh) -> usize {
        session.refresh_library_tracks(&HashMap::from([(track_id.to_string(), update)]))
    }

    #[test]
    fn a_renamed_track_is_re_resolved_in_place_for_next_and_eos() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("local", "a"), item("local", "b"), item("local", "c")],
            0,
        ));
        // Enter shuffle so the cycle's index bookkeeping is live.
        assert!(session.advance(RepeatMode::Off, true).is_some());
        let cursor = session.current_index;
        let shuffle = session.shuffle.clone().expect("shuffle state exists");

        assert_eq!(
            refresh(&mut session, "b", renamed("file:///music/renamed/b.flac")),
            1
        );

        assert_eq!(session.queue[1].uri, "file:///music/renamed/b.flac");
        assert_eq!(session.queue[0].uri, "https://media.invalid/a");
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
    fn a_refresh_reaches_the_item_playing_right_now() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a"), item("local", "b")], 1));

        assert_eq!(
            refresh(&mut session, "b", renamed("file:///music/renamed/b.flac")),
            1
        );

        // Repeat-one replays the current item from the queue, not from the view.
        assert_eq!(
            session.current().expect("current item").uri(),
            "file:///music/renamed/b.flac"
        );
    }

    #[test]
    fn a_playlist_queue_follows_the_same_library_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![item("playlist:favourites", "a"), item("local", "a")],
            0,
        ));
        assert_eq!(
            session.library_track_ids(),
            HashSet::from(["a"]),
            "duplicate playlist/local occurrences need one snapshot lookup"
        );

        assert_eq!(
            refresh(&mut session, "a", renamed("file:///music/renamed/a.flac")),
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
            session.library_track_ids(),
            HashSet::from(["a"]),
            "only local-library sources participate in snapshot filtering"
        );

        // "a" is a library UUID here, but a remote backend's native ID — and an
        // external file's URI — are namespaced by their own source.
        assert_eq!(
            refresh(&mut session, "a", renamed("file:///music/renamed/a.flac")),
            1
        );
        assert_eq!(session.queue[0].uri, "file:///downloads/a.flac");
        assert_eq!(session.queue[1].uri, "https://media.invalid/a");
        assert_eq!(session.queue[2].uri, "file:///music/renamed/a.flac");
    }

    #[test]
    fn an_unplayable_update_never_strands_a_queued_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a")], 0));

        assert_eq!(refresh(&mut session, "a", renamed("")), 0);
        assert_eq!(
            session.queue[0].uri, "https://media.invalid/a",
            "a track with no playable URI keeps the reference the queue captured"
        );
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
        assert_eq!(session.current_identity().unwrap().track_id, "c");
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
        assert_eq!(session.current_identity().unwrap().track_id, "b");
    }

    #[test]
    fn source_navigation_does_not_change_playing_source_or_track() {
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "track-a")], 0));

        let active_view_source = "remote-server";
        assert_ne!(
            session.current_identity().unwrap().source_id,
            active_view_source
        );
        assert_eq!(session.current_identity().unwrap().source_id, "local");
        assert_eq!(session.current_identity().unwrap().track_id, "track-a");
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
        assert_eq!(session.current_identity().unwrap().track_id, "c");
        assert_eq!(session.advance(RepeatMode::All, false), Some(0));
        assert_eq!(session.current_identity().unwrap().track_id, "a");
        assert_eq!(session.previous(RepeatMode::All, false), Some(2));
        assert_eq!(session.current_identity().unwrap().track_id, "c");
    }

    #[test]
    fn eos_repeat_policies_are_bound_to_the_snapshot_cursor() {
        let queue = vec![item("source", "a"), item("source", "b")];

        let mut repeat_one = PlaybackSession::default();
        assert!(repeat_one.replace_queue(queue.clone(), 1));
        // EOS repeat-one calls replay_current and does not move the cursor.
        let replayed = repeat_one.current_identity().cloned();
        assert_eq!(replayed.unwrap().track_id, "b");

        let mut repeat_off = PlaybackSession::default();
        assert!(repeat_off.replace_queue(queue.clone(), 1));
        assert_eq!(repeat_off.advance(RepeatMode::Off, false), None);

        let mut repeat_all = PlaybackSession::default();
        assert!(repeat_all.replace_queue(queue, 1));
        assert_eq!(repeat_all.advance(RepeatMode::All, false), Some(0));
        assert_eq!(repeat_all.current_identity().unwrap().track_id, "a");
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
            visited.insert(session.current_identity().unwrap().track_id.clone());
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
        let previous_id = session.current_identity().unwrap().track_id.clone();
        assert!(session.advance(RepeatMode::Off, true).is_some());
        assert!(session.previous(RepeatMode::Off, true).is_some());
        assert_eq!(session.current_identity().unwrap().track_id, previous_id);
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
        assert!(session.replace_queue(vec![external], 0));
        assert_eq!(session.current_identity().unwrap().source_id, "external");
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
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![item("local", "a")], 0));
        assert_eq!(
            resolve_play_request(session.has_current(), 3, false),
            PlayRequest::Resume
        );

        session.clear();

        assert_eq!(
            resolve_play_request(session.has_current(), 3, false),
            PlayRequest::StartAt(0)
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

        assert_eq!(session.current_identity().unwrap().track_id, "same-track");
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
