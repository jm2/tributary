//! "Copy to Device" menu entries and the copy job's toasts.
//!
//! Menus offer only removable devices whose filesystem was last reported
//! writable. The copy runs on a blocking worker; a toast shows its progress
//! with a Cancel button, and a second toast summarizes the outcome.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::objects::{SourceObject, TrackObject};
use crate::architecture::{SourceId, TrackId};
use crate::device::copy::{self, CopyProgress, CopyRefusal, CopyRequest, CopySource, CopySummary};

/// A mounted, writable device offered as a copy destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyDestination {
    pub name: String,
    pub source_key: String,
    pub mount_point: PathBuf,
}

/// Where a copy job reports its progress and outcome.
#[derive(Clone)]
pub struct CopyFeedback {
    pub toast_overlay: adw::ToastOverlay,
    pub rt_handle: tokio::runtime::Handle,
    pub sidebar_store: gio::ListStore,
}

fn device_rows(sidebar_store: &gio::ListStore) -> impl Iterator<Item = SourceObject> + '_ {
    (0..sidebar_store.n_items())
        .filter_map(|position| sidebar_store.item(position).and_downcast::<SourceObject>())
        .filter(|source| source.backend_type() == "usb-device")
}

/// The removable devices in the sidebar whose filesystem is writable.
pub fn writable_devices(sidebar_store: &gio::ListStore) -> Vec<CopyDestination> {
    device_rows(sidebar_store)
        .filter(SourceObject::device_writable)
        .filter_map(|row| {
            Some(CopyDestination {
                mount_point: row.device_mount_point()?,
                name: row.name(),
                source_key: row.source_key(),
            })
        })
        .collect()
}

/// Record on `row`, without blocking the UI, whether its device's
/// filesystem accepts writes.
pub fn probe_writable(row: &SourceObject) {
    let Some(mount_point) = row.device_mount_point() else {
        return;
    };
    let row = row.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let info = gio::File::for_path(&mount_point)
            .query_filesystem_info_future(
                gio::FILE_ATTRIBUTE_FILESYSTEM_READONLY,
                glib::Priority::DEFAULT,
            )
            .await;
        if let Some(row) = row.upgrade() {
            row.set_device_writable(
                info.is_ok_and(|info| !info.boolean(gio::FILE_ATTRIBUTE_FILESYSTEM_READONLY)),
            );
        }
    });
}

/// The files behind the selected rows, and how many rows have none.
///
/// A local row names its file by URI. A removable row names it by its
/// mount-relative identity beneath its device's current mount point.
pub fn selection_sources(
    model: &impl IsA<gio::ListModel>,
    positions: &[u32],
    sidebar_store: &gio::ListStore,
) -> (Vec<CopySource>, usize) {
    let device_roots: HashMap<SourceId, PathBuf> = device_rows(sidebar_store)
        .filter_map(|row| Some((row.source_id()?, row.device_mount_point()?)))
        .collect();
    let mut sources = Vec::with_capacity(positions.len());
    let mut unavailable = 0;
    for &position in positions {
        let track = model.item(position).and_downcast::<TrackObject>();
        let Some((track, path)) = track.and_then(|track| {
            let path = track_file(&track, &device_roots)?;
            Some((track, path))
        }) else {
            unavailable += 1;
            continue;
        };
        sources.push(CopySource {
            path,
            album_artist: track.album_artist(),
            artist: track.artist(),
            album: track.album(),
            title: track.title(),
            track_number: track.track_number(),
        });
    }
    (sources, unavailable)
}

fn track_file(track: &TrackObject, device_roots: &HashMap<SourceId, PathBuf>) -> Option<PathBuf> {
    if let Some(root) = track.source_id().and_then(|id| device_roots.get(&id)) {
        let relative = TrackId::new(track.track_id())
            .ok()?
            .removable_relative_path()
            .ok()?;
        return Some(root.join(relative));
    }
    super::context_menu::local_file_path(&track.uri())
}

/// Append a "Copy to Device" heading and one entry per destination.
///
/// `namespace` prefixes the action names the menu items refer to.
pub fn append_menu_items(
    menu: &gio::Menu,
    action_group: &gio::SimpleActionGroup,
    namespace: &str,
    destinations: Vec<CopyDestination>,
    on_activate: &Rc<dyn Fn(CopyDestination)>,
) {
    if destinations.is_empty() {
        return;
    }
    // A disabled action renders as an unclickable heading.
    let heading = gio::SimpleAction::new("copy-to-device-heading", None);
    heading.set_enabled(false);
    action_group.add_action(&heading);
    menu.append(
        Some(rust_i18n::t!("device_copy.menu_heading").as_ref()),
        Some(&format!("{namespace}copy-to-device-heading")),
    );
    for (index, destination) in destinations.into_iter().enumerate() {
        let name = format!("copy-to-device-{index}");
        let action = gio::SimpleAction::new(&name, None);
        let label = format!("  {}", destination.name);
        let on_activate = Rc::clone(on_activate);
        action.connect_activate(move |_, _| on_activate(destination.clone()));
        action_group.add_action(&action);
        menu.append(Some(&label), Some(&format!("{namespace}{name}")));
    }
}

/// Copy already-resolved track files to `destination`.
pub fn copy_tracks(
    feedback: &CopyFeedback,
    destination: CopyDestination,
    sources: Vec<CopySource>,
    unavailable: usize,
) {
    start(feedback, destination, None, async move {
        Some((sources, unavailable))
    });
}

/// Copy a local regular or smart playlist and write its `.m3u8`.
pub fn copy_playlist(
    feedback: &CopyFeedback,
    destination: CopyDestination,
    playlist_id: String,
    playlist_name: String,
) {
    start(feedback, destination, Some(playlist_name), async move {
        match load_playlist(&playlist_id).await {
            Ok(Some((tracks, others))) => {
                Some((tracks.into_iter().map(model_source).collect(), others))
            }
            Ok(None) => None,
            Err(error) => {
                tracing::error!(%error, "Could not read the playlist to copy");
                None
            }
        }
    });
}

async fn load_playlist(
    playlist_id: &str,
) -> Result<Option<(Vec<crate::db::entities::track::Model>, usize)>, sea_orm::DbErr> {
    let db = crate::db::connection::init_db().await?;
    let manager = crate::local::playlist_manager::PlaylistManager::new(db);
    let Some(playlist) = manager.get_playlist(playlist_id).await? else {
        return Ok(None);
    };
    if playlist.is_smart {
        let tracks = manager.evaluate_smart_playlist(playlist_id).await?;
        Ok(Some((tracks, 0)))
    } else {
        manager.local_playlist_tracks(playlist_id).await.map(Some)
    }
}

fn model_source(track: crate::db::entities::track::Model) -> CopySource {
    CopySource {
        path: PathBuf::from(track.file_path),
        album_artist: track.album_artist_name.unwrap_or_default(),
        artist: track.artist_name,
        album: track.album_title,
        title: track.title,
        track_number: track
            .track_number
            .and_then(|number| u32::try_from(number).ok())
            .unwrap_or(0),
    }
}

enum CopyEvent {
    Progress(CopyProgress),
    Finished(Result<CopySummary, CopyFailure>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyFailure {
    Refused(CopyRefusal),
    PlaylistUnreadable,
}

fn start(
    feedback: &CopyFeedback,
    destination: CopyDestination,
    playlist_name: Option<String>,
    sources: impl Future<Output = Option<(Vec<CopySource>, usize)>> + Send + 'static,
) {
    let device = destination.name.clone();
    // The menu may have stayed open while the device was removed or remounted.
    if !writable_devices(&feedback.sidebar_store).contains(&destination) {
        show_toast(
            &feedback.toast_overlay,
            &outcome_message(
                &device,
                Some(Err(CopyFailure::Refused(CopyRefusal::DeviceUnavailable))),
            ),
        );
        return;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let progress_toast = adw::Toast::builder()
        .title(rust_i18n::t!("device_copy.starting", device = device).as_ref())
        .use_markup(false)
        .timeout(0)
        .button_label(rust_i18n::t!("dialogs.cancel").as_ref())
        .build();
    {
        let cancel = Arc::clone(&cancel);
        progress_toast.connect_button_clicked(move |_| cancel.store(true, Ordering::Relaxed));
    }
    feedback.toast_overlay.add_toast(progress_toast.clone());

    let (events, received) = async_channel::unbounded();
    let device_root = destination.mount_point;
    let unknown_artist = rust_i18n::t!("device_copy.unknown_artist").into_owned();
    let unknown_album = rust_i18n::t!("device_copy.unknown_album").into_owned();
    feedback.rt_handle.spawn(async move {
        let Some((sources, unavailable)) = sources.await else {
            let failure = Err(CopyFailure::PlaylistUnreadable);
            let _ = events.send(CopyEvent::Finished(failure)).await;
            return;
        };
        let request = CopyRequest {
            device_root,
            sources,
            playlist_name,
            unknown_artist,
            unknown_album,
        };
        let progress_events = events.clone();
        let worker = tokio::task::spawn_blocking(move || {
            copy::run(&request, copy::query_device_space, &cancel, |progress| {
                let _ = progress_events.send_blocking(CopyEvent::Progress(progress));
            })
        });
        // A panicked worker drops `events`, which the UI reports as a stop.
        if let Ok(result) = worker.await {
            let result = result
                .map(|summary| CopySummary {
                    failed: summary.failed + unavailable,
                    ..summary
                })
                .map_err(CopyFailure::Refused);
            let _ = events.send(CopyEvent::Finished(result)).await;
        }
    });

    let toast_overlay = feedback.toast_overlay.clone();
    glib::MainContext::default().spawn_local(async move {
        let mut outcome = None;
        let mut shown_files = None;
        while let Ok(event) = received.recv().await {
            match event {
                CopyEvent::Progress(progress) => {
                    if shown_files != Some(progress.files_done) {
                        shown_files = Some(progress.files_done);
                        progress_toast.set_title(&rust_i18n::t!(
                            "device_copy.progress",
                            device = device,
                            done = progress.files_done,
                            total = progress.files_total
                        ));
                    }
                }
                CopyEvent::Finished(result) => {
                    outcome = Some(result);
                    break;
                }
            }
        }
        progress_toast.dismiss();
        show_toast(&toast_overlay, &outcome_message(&device, outcome));
    });
}

fn show_toast(toast_overlay: &adw::ToastOverlay, message: &str) {
    toast_overlay.add_toast(
        adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .build(),
    );
}

/// The summary toast text; `None` means the worker stopped without a result.
fn outcome_message(device: &str, outcome: Option<Result<CopySummary, CopyFailure>>) -> String {
    let summary = |key: &str, summary: CopySummary| {
        rust_i18n::t!(
            key,
            device = device,
            copied = summary.copied,
            skipped = summary.skipped,
            failed = summary.failed
        )
    };
    match outcome {
        Some(Ok(done)) if done.cancelled => summary("device_copy.cancelled", done),
        Some(Ok(done)) => summary("device_copy.finished", done),
        Some(Err(CopyFailure::Refused(CopyRefusal::InsufficientSpace {
            required,
            available,
        }))) => rust_i18n::t!(
            "device_copy.not_enough_space",
            device = device,
            required = glib::format_size(required),
            available = glib::format_size(available)
        ),
        Some(Err(CopyFailure::Refused(CopyRefusal::ReadOnly))) => {
            rust_i18n::t!("device_copy.read_only", device = device)
        }
        Some(Err(CopyFailure::Refused(CopyRefusal::DeviceUnavailable))) => {
            rust_i18n::t!("device_copy.device_unavailable", device = device)
        }
        Some(Err(CopyFailure::PlaylistUnreadable)) => {
            rust_i18n::t!("device_copy.playlist_unreadable")
        }
        None => rust_i18n::t!("device_copy.stopped", device = device),
    }
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_row(name: &str, key: &str, mount: &str, writable: bool) -> SourceObject {
        let row = SourceObject::removable_device(name, key, PathBuf::from(mount));
        row.set_device_writable(writable);
        row
    }

    #[test]
    fn only_writable_device_rows_are_destinations() {
        let store = gio::ListStore::new::<SourceObject>();
        store.append(&SourceObject::source(
            "Local",
            "local",
            "folder-music-symbolic",
        ));
        store.append(&device_row("Stick", "usb:uuid:a", "/media/stick", true));
        store.append(&device_row("Card", "usb:uuid:b", "/media/card", false));

        assert_eq!(
            writable_devices(&store),
            vec![CopyDestination {
                name: "Stick".to_owned(),
                source_key: "usb:uuid:a".to_owned(),
                mount_point: PathBuf::from("/media/stick"),
            }]
        );
    }

    #[test]
    fn selection_resolves_local_and_removable_files_and_counts_the_rest() {
        let store = gio::ListStore::new::<SourceObject>();
        let device = device_row("Stick", "usb:uuid:a", "/media/stick", false);
        store.append(&device);
        let device_id = device.source_id().expect("removable source id");

        let track = |uri: &str| {
            TrackObject::new(
                2, "Song", 0, "Artist", "Album", "", "", 0, "", 0, 0, 0, "", uri,
            )
        };
        let local_path = std::env::temp_dir().join("a b.flac");
        let local_uri = url::Url::from_file_path(&local_path).expect("file URI");
        let local = track(local_uri.as_str());
        local.set_album_artist("Band");
        let mount = PathBuf::from("/media/stick");
        let on_device = mount.join("Music").join("song.mp3");
        let removable = track("");
        assert!(removable.set_source_id(device_id));
        let track_id = TrackId::removable_relative(&mount, &on_device).expect("removable identity");
        removable.set_track_id(track_id.as_str());
        let remote = track("https://example.com/stream/1");

        let tracks = gio::ListStore::new::<TrackObject>();
        tracks.extend_from_slice(&[local, removable, remote]);
        let (sources, unavailable) = selection_sources(&tracks, &[0, 1, 2], &store);

        assert_eq!(unavailable, 1);
        assert_eq!(
            sources
                .iter()
                .map(|source| source.path.clone())
                .collect::<Vec<_>>(),
            vec![local_path, on_device]
        );
        assert_eq!(sources[0].album_artist, "Band");
        assert_eq!(sources[0].track_number, 2);
    }
}
