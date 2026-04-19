//! Playback context and track navigation helpers.
//!
//! This module provides:
//! - [`PlaybackContext`] — shared state passed to playback functions
//! - [`play_track_at`] — load and play a specific track by position
//! - [`advance_track`] — move to the next track (shuffle/repeat aware)
//! - [`format_ms`] — format milliseconds as `m:ss` or `h:mm:ss`

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use tracing::warn;

use crate::audio::output::AudioOutput;
use crate::ui::header_bar::RepeatMode;
use crate::ui::objects::TrackObject;

use super::album_art;

/// Shared state for playback operations.
///
/// Passed to [`play_track_at`] and [`advance_track`] so they can load
/// tracks, update the now-playing UI, and track the current position.
pub struct PlaybackContext {
    pub model: gtk::SortListModel,
    pub active_output: Rc<RefCell<Box<dyn AudioOutput>>>,
    pub album_art: gtk::Image,
    pub title_label: gtk::Label,
    pub artist_label: gtk::Label,
    pub media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>>,
    pub current_pos: Rc<Cell<Option<u32>>>,
}

/// Try to play the track at `position` in the given model.
///
/// Uses the `SortListModel` so positions match the visible sorted order.
/// Updates the now-playing labels, the OS media overlay metadata, and
/// the `current_pos` tracker.  Returns `true` on success.
pub fn play_track_at(position: u32, ctx: &PlaybackContext) -> bool {
    let Some(item) = ctx.model.item(position) else {
        return false;
    };
    let Some(track) = item.downcast_ref::<TrackObject>() else {
        return false;
    };
    let uri = track.uri();
    if uri.is_empty() {
        warn!("Track has no playable URI");
        return false;
    }

    tracing::debug!("Playing track");

    ctx.active_output.borrow().load_uri(&uri);
    ctx.title_label.set_label(&track.title());
    ctx.artist_label
        .set_label(&format!("{} \u{2014} {}", track.artist(), track.album()));
    ctx.current_pos.set(Some(position));

    // ── Update album art ─────────────────────────────────────────
    let cover_art_url = track.cover_art_url();
    if !cover_art_url.is_empty() {
        // Remote track with a cover art URL — fetch asynchronously.
        album_art::fetch_remote_album_art(&ctx.album_art, &cover_art_url);
    } else {
        // Local track — extract from embedded tags.
        album_art::update_album_art(&ctx.album_art, &uri);
    }

    if let Some(ref mut ctrl) = *ctx.media_ctrl.borrow_mut() {
        ctrl.update_metadata(&track.title(), &track.artist(), &track.album());
    }

    true
}

/// Advance to the next track, respecting shuffle and repeat-all.
///
/// Returns `true` if a new track was loaded, `false` if we've reached
/// the end (caller should reset to idle).
pub fn advance_track(ctx: &PlaybackContext, repeat_mode: RepeatMode, shuffle: bool) -> bool {
    let n = ctx.model.n_items();
    if n == 0 {
        return false;
    }

    if shuffle {
        // Pick a random track, avoiding the current one if possible.
        let pos = if n > 1 {
            let cur = ctx.current_pos.get().unwrap_or(u32::MAX);
            loop {
                let r = fastrand::u32(..n);
                if r != cur {
                    break r;
                }
            }
        } else {
            0
        };
        return play_track_at(pos, ctx);
    }

    // Sequential advance.
    let Some(pos) = ctx.current_pos.get() else {
        return play_track_at(0, ctx);
    };

    let next = pos + 1;
    if next < n {
        play_track_at(next, ctx)
    } else if repeat_mode == RepeatMode::All && n > 0 {
        play_track_at(0, ctx)
    } else {
        false
    }
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
