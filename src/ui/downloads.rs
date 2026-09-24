//! The tracklist's Download action: queues remote server tracks for the
//! offline downloader and reports progress in a toast.
//!
//! One batch runs at a time. Downloading more tracks while a batch runs adds
//! them to it, so the progress toast and the concurrency limit cover every
//! queued track.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::objects::TrackObject;
use super::preferences::AppConfig;
use crate::architecture::{SourceId, TrackId};
use crate::download::{DownloadItem, DownloadOutcome, TrackNaming};
use crate::source_registry::SourceRegistry;

/// Sidebar backend types whose tracks come from an authenticated server.
const REMOTE_SERVER_BACKENDS: [&str; 4] = ["subsonic", "jellyfin", "plex", "daap"];

/// A selected row that can be downloaded, captured when the menu opens.
#[derive(Clone, Debug)]
pub(super) struct RemoteTrack {
    source_id: SourceId,
    session_epoch: u64,
    track_id: TrackId,
    naming: TrackNaming,
    /// Whether the source is Subsonic-family, whose `download.view` is
    /// subject to the account's download permission.
    subsonic: bool,
}

/// The sources in the sidebar that are remote servers, by exact identity,
/// each with whether it is Subsonic-family.
pub(super) fn remote_server_sources(
    sidebar_store: &gtk::gio::ListStore,
) -> HashMap<SourceId, bool> {
    (0..sidebar_store.n_items())
        .filter_map(|position| {
            sidebar_store
                .item(position)
                .and_downcast::<super::objects::SourceObject>()
        })
        .filter_map(|source| {
            let backend = source.backend_type();
            if !REMOTE_SERVER_BACKENDS.contains(&backend.as_str()) {
                return None;
            }
            Some((source.source_id()?, backend == "subsonic"))
        })
        .collect()
}

/// The download request for one row, or `None` for local, radio, device,
/// and unavailable rows.
pub(super) fn remote_track(
    track: &TrackObject,
    remote_sources: &HashMap<SourceId, bool>,
) -> Option<RemoteTrack> {
    let source_id = track.source_id()?;
    let subsonic = *remote_sources.get(&source_id)?;
    Some(RemoteTrack {
        source_id,
        subsonic,
        session_epoch: track.source_session_epoch()?,
        track_id: TrackId::remote(track.track_id()).ok()?,
        naming: TrackNaming {
            album_artist: track.album_artist(),
            artist: track.artist(),
            album: track.album(),
            title: track.title(),
            disc_number: track.disc_number(),
            track_number: track.track_number(),
        },
    })
}

/// Tally of one batch's finished downloads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    downloaded: usize,
    skipped: usize,
    /// Every failure, refusals included.
    failed: usize,
    /// Failures the server refused.
    refused: usize,
    /// Whether a Subsonic-family server refused one, so the account's
    /// download permission is the likely cause.
    subsonic_refused: bool,
    cancelled: usize,
}

impl Tally {
    const fn record(&mut self, outcome: DownloadOutcome, subsonic: bool) {
        match outcome {
            DownloadOutcome::Downloaded => self.downloaded += 1,
            DownloadOutcome::Skipped => self.skipped += 1,
            DownloadOutcome::Failed => self.failed += 1,
            DownloadOutcome::Refused => {
                self.failed += 1;
                self.refused += 1;
                self.subsonic_refused |= subsonic;
            }
            DownloadOutcome::Cancelled => self.cancelled += 1,
        }
    }

    const fn done(&self) -> usize {
        self.downloaded + self.skipped + self.failed + self.cancelled
    }
}

struct Batch {
    folder: PathBuf,
    queue: async_channel::Sender<DownloadItem>,
    cancel: CancellationToken,
    toast: adw::Toast,
    /// Destinations already queued, so a track selected twice (or two rows
    /// naming the same file) is downloaded once.
    queued: HashSet<PathBuf>,
    /// Sources of queued Subsonic-family tracks.
    subsonic_sources: HashSet<SourceId>,
    total: usize,
    tally: Tally,
}

impl Batch {
    fn show_progress(&self) {
        self.toast.set_title(&rust_i18n::t!(
            "download.progress",
            done = self.tally.done(),
            total = self.total
        ));
    }
}

/// Per-window download state shared by every context-menu popup.
#[derive(Clone)]
pub(super) struct Downloads {
    batch: Rc<RefCell<Option<Batch>>>,
    toast_overlay: adw::ToastOverlay,
    rt_handle: tokio::runtime::Handle,
    source_registry: SourceRegistry,
    config: Rc<RefCell<AppConfig>>,
}

impl Downloads {
    pub(super) fn new(
        toast_overlay: adw::ToastOverlay,
        rt_handle: tokio::runtime::Handle,
        source_registry: SourceRegistry,
        config: Rc<RefCell<AppConfig>>,
    ) -> Self {
        Self {
            batch: Rc::default(),
            toast_overlay,
            rt_handle,
            source_registry,
            config,
        }
    }

    /// Queue `tracks`, starting a batch when none is running.
    pub(super) fn download(&self, tracks: &[RemoteTrack]) {
        if tracks.is_empty() {
            return;
        }
        let idle = self.batch.borrow().is_none();
        if idle && !self.start_batch() {
            let failed = Tally {
                failed: tracks.len(),
                ..Tally::default()
            };
            self.show_toast(&summary("download.finished", failed));
            return;
        }
        let mut batch = self.batch.borrow_mut();
        let Some(batch) = batch.as_mut() else {
            return;
        };
        for track in tracks {
            let stem = batch.folder.join(track.naming.relative_stem());
            if !batch.queued.insert(stem.clone()) {
                continue;
            }
            let item = DownloadItem {
                source_id: track.source_id,
                session_epoch: track.session_epoch,
                track_id: track.track_id.clone(),
                stem,
            };
            if batch.queue.try_send(item).is_ok() {
                batch.total += 1;
                if track.subsonic {
                    batch.subsonic_sources.insert(track.source_id);
                }
            }
        }
        batch.show_progress();
    }

    fn start_batch(&self) -> bool {
        let folder = super::preferences::download_dir(&self.config.borrow());
        let http = crate::audio::cast_http_server::UpstreamMediaClient::new();
        let (Some(folder), Ok(http)) = (folder, http) else {
            warn!("Downloads need a music folder and an HTTP client; one is unavailable");
            return false;
        };
        let (queue, queue_rx) = async_channel::unbounded();
        let (outcome_tx, outcomes) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        self.rt_handle.spawn(crate::download::run_queue(
            self.source_registry.clone(),
            http,
            queue_rx,
            cancel.clone(),
            outcome_tx,
        ));

        let toast = adw::Toast::builder()
            .use_markup(false)
            .timeout(0)
            .button_label(rust_i18n::t!("dialogs.cancel").as_ref())
            .build();
        {
            let cancel = cancel.clone();
            toast.connect_button_clicked(move |_| cancel.cancel());
        }
        self.toast_overlay.add_toast(toast.clone());
        *self.batch.borrow_mut() = Some(Batch {
            folder,
            queue,
            cancel,
            toast,
            queued: HashSet::new(),
            subsonic_sources: HashSet::new(),
            total: 0,
            tally: Tally::default(),
        });

        let downloads = self.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            while let Ok((source_id, outcome)) = outcomes.recv().await {
                if downloads.record(source_id, outcome) {
                    break;
                }
            }
        });
        true
    }

    /// Count one outcome; returns whether that finished the batch.
    fn record(&self, source_id: SourceId, outcome: DownloadOutcome) -> bool {
        let finished = {
            let mut slot = self.batch.borrow_mut();
            let Some(batch) = slot.as_mut() else {
                return true;
            };
            let subsonic = batch.subsonic_sources.contains(&source_id);
            batch.tally.record(outcome, subsonic);
            batch.show_progress();
            if batch.tally.done() < batch.total {
                return false;
            }
            // Dropping the batch closes its queue, which ends the workers.
            slot.take()
        };
        if let Some(batch) = finished {
            self.finish(&batch);
        }
        true
    }

    fn finish(&self, batch: &Batch) {
        batch.toast.dismiss();
        let key = if batch.cancel.is_cancelled() {
            "download.cancelled"
        } else {
            "download.finished"
        };
        self.show_toast(&summary(key, batch.tally));
        if let Some(notice) = refusal_notice(&rust_i18n::locale(), batch.tally) {
            self.show_toast(&notice);
        }
        if batch.tally.downloaded > 0
            && super::preferences::add_download_library_path(&self.config, &batch.folder)
        {
            self.show_toast(&rust_i18n::t!("preferences.library_restart_hint"));
        }
    }

    fn show_toast(&self, title: &str) {
        self.toast_overlay
            .add_toast(adw::Toast::builder().title(title).use_markup(false).build());
    }
}

fn summary(key: &str, tally: Tally) -> String {
    rust_i18n::t!(
        key,
        downloaded = tally.downloaded,
        skipped = tally.skipped,
        failed = tally.failed
    )
    .into_owned()
}

/// The toast after the summary when the server refused downloads. It is a
/// toast of its own because a toast title is one ellipsized line, too short
/// for the summary and this together.
fn refusal_notice(locale: &str, tally: Tally) -> Option<String> {
    if tally.refused == 0 {
        return None;
    }
    let base = if tally.subsonic_refused {
        "download.refused_permission"
    } else {
        "download.refused"
    };
    let key = super::l10n::plural_key(base, locale, &tally.refused.to_string());
    Some(rust_i18n::t!(key.as_str(), locale = locale, count = tally.refused).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rows_of_remote_servers_with_a_session_are_downloadable() {
        let (server, other_server) = (SourceId::random(), SourceId::random());
        let remote_sources = HashMap::from([(server, true), (other_server, false)]);
        let row = |source_id: SourceId, epoch: Option<u64>| {
            let track = TrackObject::new(
                4, "Song", 0, "Artist", "Album", "", "", 0, "", 0, 0, 0, "flac", "",
            );
            track.set_track_id("song-4");
            assert!(track.set_source_id(source_id));
            if let Some(epoch) = epoch {
                track.set_source_session_epoch(epoch);
            }
            track
        };

        let remote = remote_track(&row(server, Some(7)), &remote_sources).expect("remote row");
        assert_eq!(remote.source_id, server);
        assert!(remote.subsonic);
        assert_eq!(remote.session_epoch, 7);
        assert_eq!(remote.track_id.as_str(), "song-4");
        assert_eq!(
            remote.naming.relative_stem(),
            std::path::Path::new("Artist/Album/04 Song")
        );
        let other = remote_track(&row(other_server, Some(2)), &remote_sources).expect("remote row");
        assert!(!other.subsonic);
        assert!(remote_track(&row(server, None), &remote_sources).is_none());
        assert!(remote_track(&row(SourceId::local(), Some(7)), &remote_sources).is_none());
        assert!(remote_track(&row(SourceId::radio_browser(), Some(7)), &remote_sources).is_none());
    }

    #[test]
    fn refusals_add_a_notice_after_the_summary() {
        let mut tally = Tally::default();
        tally.record(DownloadOutcome::Downloaded, true);
        tally.record(DownloadOutcome::Failed, true);
        assert_eq!(
            summary("download.finished", tally),
            "Downloads finished. Downloaded: 1, skipped: 0, failed: 1."
        );
        assert_eq!(refusal_notice("en", tally), None, "other failures");

        tally.record(DownloadOutcome::Refused, false);
        assert_eq!(
            summary("download.cancelled", tally),
            "Downloads cancelled. Downloaded: 1, skipped: 0, failed: 2."
        );
        assert_eq!(
            refusal_notice("en", tally).as_deref(),
            Some("The server refused 1 download.")
        );

        tally.record(DownloadOutcome::Refused, true);
        assert_eq!(
            refusal_notice("en", tally).as_deref(),
            Some(
                "The server refused 2 downloads. \
                 Ask its administrator to allow downloads for this account."
            ),
            "a Subsonic-family refusal names the download permission"
        );

        for locale in rust_i18n::available_locales!() {
            for count in [1, 2, 5, 22] {
                let notice = |subsonic_refused| {
                    let tally = Tally {
                        failed: count,
                        refused: count,
                        subsonic_refused,
                        ..Tally::default()
                    };
                    refusal_notice(&locale, tally).expect("notice")
                };
                let (plain, permission) = (notice(false), notice(true));
                assert_ne!(plain, permission, "{locale} {count}");
                for notice in [plain, permission] {
                    assert!(
                        notice.contains(&count.to_string()) && !notice.contains("%{"),
                        "{locale} {count}: {notice}"
                    );
                }
            }
        }
    }
}
