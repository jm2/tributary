//! OS-level media transport controls via the `souvlaki` crate.
//!
//! Provides a [`MediaController`] that bridges the host OS media overlay
//! (MPRIS on Linux, SMTC on Windows, Now Playing on macOS) to the
//! application through an [`async_channel`].
//!
//! # Threading model
//!
//! `souvlaki` invokes its event callback from an internal thread.  The
//! callback forwards [`MediaAction`]s through an [`async_channel::Sender`]
//! (which is `Send`) so that the GTK main thread receives them safely
//! via the [`async_channel::Receiver`] returned by [`MediaController::new`].
//!
//! Every [`MediaController`] method must be called from the GTK main thread.
//!
//! # Publishing
//!
//! The overlay's seek bar needs the track length, carried in the metadata,
//! and the position, carried in the playback state. The position is
//! republished every [`POSITION_REFRESH`] while playing, for MPRIS clients
//! that read it on demand instead of extrapolating, and after every seek.
//!
//! `souvlaki`'s MPRIS backend applies about one queued update per second, so
//! the controller records the wanted state and publishes only what changed,
//! once no change has arrived for [`PUBLISH_DELAY`]. A track load, or the
//! brief pause the output reports while it prerolls or seeks, then reaches the
//! OS as one update instead of a queue the overlay lags behind.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tracing::{debug, info, warn};

// ── Actions ─────────────────────────────────────────────────────────────

/// Actions received from the operating system's media transport controls.
///
/// These arrive on the GTK main thread via the [`async_channel::Receiver`]
/// returned by [`MediaController::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaAction {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    /// Move to this position in the current track, in milliseconds.
    SetPosition(u64),
    /// Move by this many milliseconds, backwards when negative.
    SeekBy(i64),
}

/// How far the Windows fast-forward and rewind buttons move, since they
/// carry no amount of their own.
const SEEK_STEP: Duration = Duration::from_secs(10);

/// How stale the published position may get while playing.
const POSITION_REFRESH: Duration = Duration::from_secs(2);

/// How long the wanted state must stay unchanged before it is published.
/// Shorter than the outputs' position interval, so position updates never
/// hold a publish back.
const PUBLISH_DELAY: Duration = Duration::from_millis(250);

/// A reported position this far from where the published one has advanced to
/// is a seek, and is published without waiting for the refresh.
const POSITION_JUMP_MS: u64 = 1_500;

/// Outputs refine a track's length while it plays. Changes smaller than this
/// are not republished.
const DURATION_TOLERANCE_MS: u64 = 1_000;

fn media_action(event: MediaControlEvent) -> Option<MediaAction> {
    Some(match event {
        MediaControlEvent::Play => MediaAction::Play,
        MediaControlEvent::Pause => MediaAction::Pause,
        MediaControlEvent::Toggle => MediaAction::Toggle,
        MediaControlEvent::Next => MediaAction::Next,
        MediaControlEvent::Previous => MediaAction::Previous,
        MediaControlEvent::Stop => MediaAction::Stop,
        MediaControlEvent::SetPosition(MediaPosition(position)) => {
            MediaAction::SetPosition(saturating_ms(position))
        }
        MediaControlEvent::SeekBy(direction, offset) => {
            MediaAction::SeekBy(signed_ms(direction, offset))
        }
        MediaControlEvent::Seek(direction) => MediaAction::SeekBy(signed_ms(direction, SEEK_STEP)),
        other => {
            debug!("Unhandled media control event: {other:?}");
            return None;
        }
    })
}

fn saturating_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn signed_ms(direction: SeekDirection, offset: Duration) -> i64 {
    let ms = i64::try_from(offset.as_millis()).unwrap_or(i64::MAX);
    match direction {
        SeekDirection::Forward => ms,
        SeekDirection::Backward => -ms,
    }
}

/// Where a relative seek from the desktop lands, following MPRIS: before the
/// start clamps to the start, and `None` (at or past the end) means move to
/// the next track.
pub fn relative_seek_target(position_ms: u64, offset_ms: i64, duration_ms: u64) -> Option<u64> {
    let target_ms = position_ms.saturating_add_signed(offset_ms);
    (target_ms < duration_ms).then_some(target_ms)
}

// ── Published state ─────────────────────────────────────────────────────

/// The track the OS overlay shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NowPlaying {
    title: String,
    artist: String,
    album: String,
    /// `None` for a live stream, which has no timeline.
    duration_ms: Option<u64>,
    cover_url: Option<String>,
}

impl NowPlaying {
    fn metadata(&self) -> MediaMetadata<'_> {
        MediaMetadata {
            title: Some(&self.title),
            artist: Some(&self.artist),
            album: Some(&self.album),
            cover_url: self.cover_url.as_deref(),
            duration: self.duration_ms.map(Duration::from_millis),
        }
    }
}

fn duration_changed(published_ms: Option<u64>, reported_ms: Option<u64>) -> bool {
    match (published_ms, reported_ms) {
        (Some(published_ms), Some(reported_ms)) => {
            published_ms.abs_diff(reported_ms) >= DURATION_TOLERANCE_MS
        }
        (None, None) => false,
        _ => true,
    }
}

/// The last position handed to the OS.
#[derive(Debug, Clone, Copy)]
struct PublishedPosition {
    at: Instant,
    position_ms: u64,
    playing: bool,
}

fn position_publish_due(last: PublishedPosition, now: Instant, position_ms: u64) -> bool {
    let elapsed = now.saturating_duration_since(last.at);
    let expected_ms = if last.playing {
        last.position_ms.saturating_add(saturating_ms(elapsed))
    } else {
        last.position_ms
    };
    (last.playing && elapsed >= POSITION_REFRESH)
        || expected_ms.abs_diff(position_ms) >= POSITION_JUMP_MS
}

/// What the overlay should show, and what it was last given.
#[derive(Debug, Default)]
struct Overlay {
    /// `None` while idle.
    now_playing: Option<NowPlaying>,
    playing: bool,
    position_ms: u64,
    published_now_playing: Option<NowPlaying>,
    published_position: Option<PublishedPosition>,
}

/// Metadata one publish hands to the OS.
#[derive(Debug, PartialEq, Eq)]
enum MetadataUpdate {
    Track(NowPlaying),
    Idle,
}

/// What one publish hands to the OS.
#[derive(Debug, Default, PartialEq, Eq)]
struct Updates {
    metadata: Option<MetadataUpdate>,
    playback: Option<MediaPlayback>,
}

impl Overlay {
    /// Take what changed since the last publish, recording it as published.
    fn take_updates(&mut self, now: Instant) -> Updates {
        let mut updates = Updates::default();
        if self.published_now_playing != self.now_playing {
            self.published_now_playing.clone_from(&self.now_playing);
            updates.metadata = Some(
                self.now_playing
                    .clone()
                    .map_or(MetadataUpdate::Idle, MetadataUpdate::Track),
            );
        }
        if self.now_playing.is_none() {
            if updates.metadata.is_some() {
                self.published_position = None;
                updates.playback = Some(MediaPlayback::Stopped);
            }
            return updates;
        }
        let due = self.published_position.is_none_or(|last| {
            last.playing != self.playing || position_publish_due(last, now, self.position_ms)
        });
        if due {
            let progress = Some(MediaPosition(Duration::from_millis(self.position_ms)));
            updates.playback = Some(if self.playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            });
            self.published_position = Some(PublishedPosition {
                at: now,
                position_ms: self.position_ms,
                playing: self.playing,
            });
        }
        updates
    }
}

/// The URL each platform backend loads a local cover from. The Windows
/// backend strips `file://` and opens the rest as a native path, so it takes
/// the path unencoded; MPRIS and macOS take a real `file:` URL.
fn cover_url(path: &Path) -> Option<String> {
    if cfg!(target_os = "windows") {
        path.to_str().map(|path| format!("file://{path}"))
    } else {
        url::Url::from_file_path(path).ok().map(String::from)
    }
}

// ── Controller ──────────────────────────────────────────────────────────

/// What the OS overlay shows while nothing plays: the application name
/// and a localized idle status.
fn idle_metadata(status: &str) -> MediaMetadata<'_> {
    MediaMetadata {
        title: Some("Tributary"),
        artist: Some(status),
        album: Some(""),
        ..Default::default()
    }
}

/// Wraps `souvlaki::MediaControls` and routes OS media-key events to
/// the GTK main thread.
pub struct MediaController {
    publisher: Rc<RefCell<Publisher>>,
}

struct Publisher {
    controls: MediaControls,
    overlay: Overlay,
    /// The pending publish, restarted by every change.
    pending: Option<glib::SourceId>,
}

impl Publisher {
    fn publish(&mut self) {
        let updates = self.overlay.take_updates(Instant::now());
        match updates.metadata {
            Some(MetadataUpdate::Track(now_playing)) => self.publish_metadata(&now_playing),
            Some(MetadataUpdate::Idle) => {
                let idle_status = rust_i18n::t!("app.not_playing");
                if let Err(e) = self.controls.set_metadata(idle_metadata(&idle_status)) {
                    warn!("Failed to clear media metadata on stop: {e:?}");
                }
            }
            None => {}
        }
        if let Some(playback) = updates.playback {
            if let Err(e) = self.controls.set_playback(playback) {
                warn!("Failed to update playback state: {e:?}");
            }
        }
    }

    fn publish_metadata(&mut self, now_playing: &NowPlaying) {
        let Err(error) = self.controls.set_metadata(now_playing.metadata()) else {
            return;
        };
        // The Windows backend loads the cover while publishing and drops the
        // whole update when that fails; the text is still worth showing.
        if now_playing.cover_url.is_some() {
            debug!("Failed to publish media metadata with its cover: {error:?}");
            let without_cover = MediaMetadata {
                cover_url: None,
                ..now_playing.metadata()
            };
            if let Err(error) = self.controls.set_metadata(without_cover) {
                warn!("Failed to update media metadata: {error:?}");
            }
        } else {
            warn!("Failed to update media metadata: {error:?}");
        }
    }
}

impl MediaController {
    /// Register with the host OS and return the controller + a receiver
    /// for incoming [`MediaAction`]s.
    ///
    /// On Linux this creates an MPRIS D-Bus service named `tributary`.
    /// On Windows, `hwnd` **must** be `Some(ptr)` pointing to the
    /// application's main window — SMTC will panic without it.
    /// On macOS, `hwnd` is ignored.
    ///
    /// The caller must consume the receiver on the GTK main thread via:
    /// ```ignore
    /// glib::MainContext::default().spawn_local(async move {
    ///     while let Ok(action) = media_rx.recv().await {
    ///         // handle MediaAction …
    ///     }
    /// });
    /// ```
    pub fn new(
        hwnd: Option<*mut std::ffi::c_void>,
    ) -> anyhow::Result<(Self, async_channel::Receiver<MediaAction>)> {
        // On Windows, souvlaki's SMTC backend does `hwnd.expect(...)`, which
        // panics (and aborts across the GLib FFI boundary, killing the app)
        // when the native surface/HWND isn't ready yet. Guard here and return
        // a recoverable error instead — the caller degrades gracefully when
        // media controls are unavailable. On Linux/macOS the hwnd is always
        // None and is ignored, so this guard is Windows-only.
        #[cfg(target_os = "windows")]
        if hwnd.is_none() {
            anyhow::bail!(
                "Windows media controls require an HWND, but the window surface is not ready yet"
            );
        }

        let config = PlatformConfig {
            dbus_name: "tributary",
            display_name: "Tributary",
            hwnd,
        };

        let mut controls = MediaControls::new(config)
            .map_err(|e| anyhow::anyhow!("Failed to create media controls: {e:?}"))?;

        info!("OS media transport controls initialised");

        // ── Event channel: souvlaki callback thread → GTK main thread ──
        let (action_tx, action_rx) = async_channel::unbounded();

        controls
            .attach(move |event: MediaControlEvent| {
                if let Some(a) = media_action(event) {
                    debug!(?a, "OS media key received");
                    let _ = action_tx.try_send(a);
                }
            })
            .map_err(|e| anyhow::anyhow!("Failed to attach media controls handler: {e:?}"))?;

        // Publish initial (idle) state so the OS overlay is registered.
        let idle_status = rust_i18n::t!("app.not_playing");
        controls
            .set_metadata(idle_metadata(&idle_status))
            .map_err(|e| anyhow::anyhow!("Failed to set initial metadata: {e:?}"))?;

        controls
            .set_playback(MediaPlayback::Stopped)
            .map_err(|e| anyhow::anyhow!("Failed to set initial playback state: {e:?}"))?;

        let publisher = Publisher {
            controls,
            overlay: Overlay::default(),
            pending: None,
        };
        Ok((
            Self {
                publisher: Rc::new(RefCell::new(publisher)),
            },
            action_rx,
        ))
    }

    /// Apply `change` to the wanted overlay state and publish it once the
    /// state settles.
    fn update(&self, change: impl FnOnce(&mut Overlay)) {
        let mut publisher = self.publisher.borrow_mut();
        change(&mut publisher.overlay);
        if let Some(pending) = publisher.pending.take() {
            pending.remove();
        }
        let weak = Rc::downgrade(&self.publisher);
        publisher.pending = Some(glib::timeout_add_local_once(PUBLISH_DELAY, move || {
            if let Some(publisher) = weak.upgrade() {
                let mut publisher = publisher.borrow_mut();
                // This source has fired and must not be removed again.
                publisher.pending = None;
                publisher.publish();
            }
        }));
    }

    // ── Outbound: app → OS overlay ──────────────────────────────────

    /// Show a newly loaded track, starting at its beginning and without a
    /// cover until [`update_cover`](Self::update_cover) provides one.
    /// `duration_ms` is `None` for a live stream.
    pub fn update_metadata(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        duration_ms: Option<u64>,
    ) {
        let now_playing = NowPlaying {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
            duration_ms: duration_ms.filter(|duration_ms| *duration_ms > 0),
            cover_url: None,
        };
        self.update(|overlay| {
            overlay.now_playing = Some(now_playing);
            overlay.position_ms = 0;
            overlay.published_position = None;
        });
    }

    /// Show `cover`, a local image file, as the current track's artwork, or
    /// no artwork.
    pub fn update_cover(&self, cover: Option<&Path>) {
        let cover_url = cover.and_then(cover_url);
        self.update(|overlay| {
            if let Some(now_playing) = overlay.now_playing.as_mut() {
                now_playing.cover_url = cover_url;
            }
        });
    }

    /// Record the output's latest position and the track length it implies.
    pub fn update_timeline(&self, position_ms: u64, duration_ms: Option<u64>) {
        self.update(|overlay| {
            let Some(now_playing) = overlay.now_playing.as_mut() else {
                return;
            };
            if duration_changed(now_playing.duration_ms, duration_ms) {
                now_playing.duration_ms = duration_ms;
            }
            overlay.position_ms = position_ms;
        });
    }

    /// The length of the current track: `None` when nothing is loaded or the
    /// track is a live stream, which cannot be seeked.
    pub fn duration_ms(&self) -> Option<u64> {
        self.publisher
            .borrow()
            .overlay
            .now_playing
            .as_ref()
            .and_then(|now_playing| now_playing.duration_ms)
    }

    /// Inform the OS whether we are currently playing or paused.
    pub fn update_playback(&self, playing: bool) {
        self.update(|overlay| overlay.playing = playing);
    }

    /// Tell the OS that playback has stopped entirely, returning the overlay
    /// to the idle metadata published at construction.
    pub fn set_stopped(&self) {
        self.update(|overlay| {
            overlay.now_playing = None;
            overlay.playing = false;
            overlay.position_ms = 0;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(duration_ms: Option<u64>, cover_url: Option<&str>) -> NowPlaying {
        NowPlaying {
            title: "Song".to_owned(),
            artist: "Artist".to_owned(),
            album: "Album".to_owned(),
            duration_ms,
            cover_url: cover_url.map(str::to_owned),
        }
    }

    #[test]
    fn metadata_carries_length_and_cover() {
        let track = track(Some(215_500), Some("file:///cache/cover.jpg"));
        let metadata = track.metadata();
        assert_eq!(metadata.title, Some("Song"));
        assert_eq!(metadata.artist, Some("Artist"));
        assert_eq!(metadata.album, Some("Album"));
        assert_eq!(metadata.duration, Some(Duration::from_millis(215_500)));
        assert_eq!(metadata.cover_url, Some("file:///cache/cover.jpg"));
    }

    #[test]
    fn live_streams_publish_no_length() {
        assert_eq!(track(None, None).metadata().duration, None);
    }

    #[test]
    fn desktop_seek_events_map_to_actions() {
        let position = MediaPosition(Duration::from_millis(61_250));
        assert_eq!(
            media_action(MediaControlEvent::SetPosition(position)),
            Some(MediaAction::SetPosition(61_250))
        );
        let offset = Duration::from_secs(5);
        assert_eq!(
            media_action(MediaControlEvent::SeekBy(SeekDirection::Forward, offset)),
            Some(MediaAction::SeekBy(5_000))
        );
        assert_eq!(
            media_action(MediaControlEvent::SeekBy(SeekDirection::Backward, offset)),
            Some(MediaAction::SeekBy(-5_000))
        );
        assert_eq!(
            media_action(MediaControlEvent::Seek(SeekDirection::Backward)),
            Some(MediaAction::SeekBy(-10_000))
        );
        assert_eq!(media_action(MediaControlEvent::Raise), None);
    }

    #[test]
    fn relative_seeks_clamp_at_the_start_and_skip_past_the_end() {
        assert_eq!(relative_seek_target(30_000, 10_000, 200_000), Some(40_000));
        assert_eq!(relative_seek_target(3_000, -10_000, 200_000), Some(0));
        assert_eq!(relative_seek_target(195_000, 10_000, 200_000), None);
    }

    #[test]
    fn position_is_republished_on_refresh_and_on_seeks() {
        let start = Instant::now();
        let half_second = start + Duration::from_millis(500);
        let playing = PublishedPosition {
            at: start,
            position_ms: 10_000,
            playing: true,
        };
        assert!(!position_publish_due(playing, half_second, 10_500));
        let refresh_ms = saturating_ms(POSITION_REFRESH);
        assert!(position_publish_due(
            playing,
            start + POSITION_REFRESH,
            10_000 + refresh_ms
        ));
        assert!(position_publish_due(playing, half_second, 60_000));
        assert!(position_publish_due(playing, half_second, 2_000));

        let paused = PublishedPosition {
            playing: false,
            ..playing
        };
        let later = start + Duration::from_secs(5);
        assert!(!position_publish_due(paused, later, 10_000));
        assert!(position_publish_due(paused, later, 30_000));
    }

    fn playing_at(position_ms: u64) -> MediaPlayback {
        MediaPlayback::Playing {
            progress: Some(MediaPosition(Duration::from_millis(position_ms))),
        }
    }

    #[test]
    fn a_track_load_publishes_its_metadata_and_start_once() {
        let now = Instant::now();
        let mut overlay = Overlay {
            now_playing: Some(track(Some(90_000), None)),
            playing: true,
            ..Overlay::default()
        };
        assert_eq!(
            overlay.take_updates(now),
            Updates {
                metadata: Some(MetadataUpdate::Track(track(Some(90_000), None))),
                playback: Some(playing_at(0)),
            }
        );
        assert_eq!(overlay.take_updates(now), Updates::default());

        // The cover arriving later republishes the metadata alone.
        if let Some(now_playing) = overlay.now_playing.as_mut() {
            now_playing.cover_url = Some("file:///cache/cover.jpg".to_owned());
        }
        let updates = overlay.take_updates(now);
        assert!(updates.metadata.is_some());
        assert_eq!(updates.playback, None);
    }

    #[test]
    fn a_pause_that_ends_before_publishing_is_not_published() {
        let now = Instant::now();
        let mut overlay = Overlay {
            now_playing: Some(track(Some(90_000), None)),
            playing: true,
            ..Overlay::default()
        };
        overlay.take_updates(now);

        overlay.playing = false;
        overlay.playing = true;
        assert_eq!(overlay.take_updates(now), Updates::default());

        overlay.playing = false;
        assert_eq!(
            overlay.take_updates(now).playback,
            Some(MediaPlayback::Paused {
                progress: Some(MediaPosition(Duration::ZERO)),
            })
        );
    }

    #[test]
    fn a_seek_publishes_the_new_position() {
        let now = Instant::now();
        let mut overlay = Overlay {
            now_playing: Some(track(Some(90_000), None)),
            playing: true,
            ..Overlay::default()
        };
        overlay.take_updates(now);

        overlay.position_ms = 60_000;
        assert_eq!(overlay.take_updates(now).playback, Some(playing_at(60_000)));
    }

    #[test]
    fn stopping_returns_to_idle_once() {
        let now = Instant::now();
        let mut overlay = Overlay {
            now_playing: Some(track(Some(90_000), None)),
            playing: true,
            ..Overlay::default()
        };
        overlay.take_updates(now);

        overlay.now_playing = None;
        overlay.playing = false;
        assert_eq!(
            overlay.take_updates(now),
            Updates {
                metadata: Some(MetadataUpdate::Idle),
                playback: Some(MediaPlayback::Stopped),
            }
        );
        assert_eq!(overlay.take_updates(now), Updates::default());
    }

    #[test]
    fn small_length_refinements_are_not_republished() {
        assert!(!duration_changed(Some(200_000), Some(200_400)));
        assert!(duration_changed(Some(200_000), Some(203_000)));
        assert!(duration_changed(None, Some(200_000)));
        assert!(!duration_changed(None, None));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn local_covers_are_published_as_file_urls() {
        assert_eq!(
            cover_url(Path::new("/cache/now playing/cover-1.jpg")).as_deref(),
            Some("file:///cache/now%20playing/cover-1.jpg")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn local_covers_are_published_as_native_windows_paths() {
        assert_eq!(
            cover_url(Path::new(r"C:\cache\now playing\cover-1.jpg")).as_deref(),
            Some(r"file://C:\cache\now playing\cover-1.jpg")
        );
    }
}
