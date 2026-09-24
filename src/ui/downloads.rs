//! The tracklist's Download action: queues remote server tracks for the
//! offline downloader and reports progress in a toast.
//!
//! One batch runs at a time. Downloading more tracks while a batch runs adds
//! them to it, so the progress toast and the concurrency limit cover every
//! queued track.

use std::cell::RefCell;
use std::collections::HashSet;
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
}

/// The sources in the sidebar that are remote servers, by exact identity.
pub(super) fn remote_server_source_ids(sidebar_store: &gtk::gio::ListStore) -> HashSet<SourceId> {
    (0..sidebar_store.n_items())
        .filter_map(|position| {
            sidebar_store
                .item(position)
                .and_downcast::<super::objects::SourceObject>()
        })
        .filter(|source| REMOTE_SERVER_BACKENDS.contains(&source.backend_type().as_str()))
        .filter_map(|source| source.source_id())
        .collect()
}

/// The download request for one row, or `None` for local, radio, device,
/// and unavailable rows.
pub(super) fn remote_track(
    track: &TrackObject,
    remote_sources: &HashSet<SourceId>,
) -> Option<RemoteTrack> {
    let source_id = track.source_id().filter(|id| remote_sources.contains(id))?;
    Some(RemoteTrack {
        source_id,
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
    failed: usize,
    cancelled: usize,
}

impl Tally {
    const fn record(&mut self, outcome: DownloadOutcome) {
        match outcome {
            DownloadOutcome::Downloaded => self.downloaded += 1,
            DownloadOutcome::Skipped => self.skipped += 1,
            DownloadOutcome::Failed => self.failed += 1,
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
            total: 0,
            tally: Tally::default(),
        });

        let downloads = self.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            while let Ok(outcome) = outcomes.recv().await {
                if downloads.record(outcome) {
                    break;
                }
            }
        });
        true
    }

    /// Count one outcome; returns whether that finished the batch.
    fn record(&self, outcome: DownloadOutcome) -> bool {
        let finished = {
            let mut slot = self.batch.borrow_mut();
            let Some(batch) = slot.as_mut() else {
                return true;
            };
            batch.tally.record(outcome);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rows_of_remote_servers_with_a_session_are_downloadable() {
        let server = SourceId::random();
        let remote_sources = HashSet::from([server]);
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
        assert_eq!(remote.session_epoch, 7);
        assert_eq!(remote.track_id.as_str(), "song-4");
        assert_eq!(
            remote.naming.relative_stem(),
            std::path::Path::new("Artist/Album/04 Song")
        );
        assert!(remote_track(&row(server, None), &remote_sources).is_none());
        assert!(remote_track(&row(SourceId::local(), Some(7)), &remote_sources).is_none());
        assert!(remote_track(&row(SourceId::radio_browser(), Some(7)), &remote_sources).is_none());
    }
}
