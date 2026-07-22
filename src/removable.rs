//! Lifecycle-owned adapter for one exact mounted removable filesystem.
//!
//! Construction walks the native mount only on a blocking worker, binds every
//! accepted file through [`MountedRootAuthority`], and publishes no playable
//! path or URI. The catalogue's opaque [`TrackId`] values losslessly encode
//! mount-relative native paths; resolution decodes only an accepted identity
//! and opens it again beneath the still-current retained mount authority.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::runtime::Handle;
use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::Track;
use crate::architecture::{SourceId, TrackId};
use crate::local::resolver::{MountedRootAuthority, ResolvedFileMedia};
use crate::source_lifecycle::{
    AdapterCloseFuture, AdapterStream, CancellationObserver, CloseAuthority, LifecycleAdapter,
};
use crate::source_registry::{
    CatalogueFuture, ManagedSourceAdapter, PlaybackAttributionCapability, StreamFuture,
};

const MAX_TAG_TEXT_BYTES: usize = 64 * 1024;

/// One mounted removable source whose catalogue and media authority share an
/// exact lifecycle generation.
pub struct RemovableMediaAdapter {
    #[cfg(test)]
    source_id: SourceId,
    authority: Arc<MountedRootAuthority>,
    tracks: Vec<Track>,
    accepted_track_ids: HashSet<TrackId>,
    runtime: Handle,
}

impl RemovableMediaAdapter {
    /// Scan one exact mounted root on the caller's blocking worker.
    ///
    /// Cancellation is cooperative between filesystem and parser operations.
    /// It is deliberately returned as `Ok(None)`: a removed or superseded
    /// mount is not a source failure and must not leave a failure row behind.
    pub fn scan(
        source_id: SourceId,
        mount_root: PathBuf,
        cancellation: &CancellationObserver,
        runtime: Handle,
    ) -> BackendResult<Option<Self>> {
        if cancellation.is_cancelled() {
            return Ok(None);
        }

        let authority = match MountedRootAuthority::acquire(&mount_root) {
            Ok(authority) => Arc::new(authority),
            Err(_) if cancellation.is_cancelled() => return Ok(None),
            Err(_) => return Err(scan_failed()),
        };
        if cancellation.is_cancelled() {
            return Ok(None);
        }

        let mut tracks = Vec::new();
        let mut accepted_track_ids = HashSet::new();
        let walker = walkdir::WalkDir::new(authority.root())
            .follow_links(false)
            .same_file_system(true)
            .sort_by_file_name()
            .into_iter();

        for entry in walker {
            if cancellation.is_cancelled() {
                return Ok(None);
            }

            let Ok(entry) = entry else {
                ensure_scan_authority(&authority, cancellation)?;
                continue;
            };
            if !entry.file_type().is_file()
                || !crate::local::tag_parser::is_audio_file(entry.path())
            {
                continue;
            }

            let Ok(track_id) = TrackId::removable_relative(authority.root(), entry.path()) else {
                continue;
            };
            let Ok(relative_path) = track_id.removable_relative_path() else {
                continue;
            };
            let extension = extension_hint(&relative_path);
            let Ok(media) = ResolvedFileMedia::from_mounted_relative_path(
                Arc::clone(&authority),
                &relative_path,
                extension,
            ) else {
                ensure_scan_authority(&authority, cancellation)?;
                continue;
            };

            let parsed = media
                .with_serialized_seekable_file(|file| {
                    crate::local::tag_parser::parse_audio_file_from_file(file, &relative_path)
                })
                .ok()
                .and_then(Result::ok);
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            ensure_scan_authority(&authority, cancellation)?;
            let Some(parsed) = parsed.filter(parsed_metadata_is_bounded) else {
                continue;
            };

            if accepted_track_ids.insert(track_id.clone()) {
                tracks.push(pathless_track(source_id, track_id, parsed));
            }
        }

        if cancellation.is_cancelled() {
            return Ok(None);
        }
        if authority.validate().is_err() {
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            return Err(scan_failed());
        }
        if cancellation.is_cancelled() {
            return Ok(None);
        }

        Ok(Some(Self {
            #[cfg(test)]
            source_id,
            authority,
            tracks,
            accepted_track_ids,
            runtime,
        }))
    }

    #[cfg(test)]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[cfg(test)]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }
}

impl LifecycleAdapter for RemovableMediaAdapter {
    fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
        Box::pin(async move {
            drop(self);
            Ok(())
        })
    }
}

impl ManagedSourceAdapter for RemovableMediaAdapter {
    fn playback_attribution_capability(&self) -> PlaybackAttributionCapability {
        PlaybackAttributionCapability::Removable
    }

    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move { Ok(self.tracks.clone()) })
    }

    fn resolve_stream(self: Arc<Self>, track_id: TrackId) -> StreamFuture {
        Box::pin(async move {
            // Membership is checked before decoding or touching the mount. A
            // well-formed relative identity that appeared after the accepted
            // scan is not authority to read that file in this session.
            if !self.accepted_track_ids.contains(&track_id) {
                return Err(resolution_failed());
            }
            let relative_path = track_id
                .removable_relative_path()
                .map_err(|_| resolution_failed())?;
            let extension = extension_hint(&relative_path);
            let authority = Arc::clone(&self.authority);
            let task = self.runtime.spawn_blocking(move || {
                authority.validate()?;
                let media = ResolvedFileMedia::from_mounted_relative_path(
                    Arc::clone(&authority),
                    &relative_path,
                    extension,
                )?;
                authority.validate()?;
                Ok::<_, std::io::Error>(media)
            });

            task.await
                .map_err(|_| resolution_failed())?
                .map(AdapterStream::File)
                .map_err(|_| resolution_failed())
        })
    }
}

fn ensure_scan_authority(
    authority: &MountedRootAuthority,
    cancellation: &CancellationObserver,
) -> BackendResult<()> {
    if cancellation.is_cancelled() {
        // The caller observes cancellation at the next cooperative boundary;
        // do not manufacture a path-bearing I/O failure in the meantime.
        return Ok(());
    }
    match authority.validate() {
        Ok(()) => Ok(()),
        Err(_) if cancellation.is_cancelled() => Ok(()),
        Err(_) => Err(scan_failed()),
    }
}

fn extension_hint(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_owned)
}

fn parsed_metadata_is_bounded(parsed: &crate::local::tag_parser::ParsedTrack) -> bool {
    let required = [
        parsed.title.as_str(),
        parsed.artist_name.as_str(),
        parsed.album_title.as_str(),
        parsed.format.as_str(),
    ];
    let optional = [
        parsed.album_artist_name.as_deref(),
        parsed.genre.as_deref(),
        parsed.composer.as_deref(),
    ];
    required
        .into_iter()
        .all(|value| !value.is_empty() && value.len() <= MAX_TAG_TEXT_BYTES)
        && optional
            .into_iter()
            .flatten()
            .all(|value| value.len() <= MAX_TAG_TEXT_BYTES)
}

fn pathless_track(
    source_id: SourceId,
    track_id: TrackId,
    parsed: crate::local::tag_parser::ParsedTrack,
) -> Track {
    let compatibility_id = Uuid::new_v5(&source_id.as_uuid(), track_id.as_str().as_bytes());
    Track {
        id: compatibility_id,
        native_track_id: Some(track_id),
        title: parsed.title,
        artist_name: parsed.artist_name,
        album_artist_name: parsed.album_artist_name,
        artist_id: None,
        album_title: parsed.album_title,
        album_id: None,
        track_number: parsed.track_number,
        disc_number: parsed.disc_number,
        duration_secs: parsed.duration_secs,
        composer: parsed.composer,
        genre: parsed.genre,
        year: parsed.year,
        file_path: None,
        stream_url: None,
        cover_art_url: None,
        date_added: None,
        date_modified: Some(parsed.date_modified),
        bitrate_kbps: parsed.bitrate_kbps,
        sample_rate_hz: parsed.sample_rate_hz,
        format: Some(parsed.format),
        play_count: None,
        rating: crate::architecture::models::TrackRating::unsupported(),
        last_played: None,
    }
}

fn scan_failed() -> BackendError {
    BackendError::Internal(anyhow::anyhow!("removable media scan failed"))
}

fn resolution_failed() -> BackendError {
    BackendError::Internal(anyhow::anyhow!("removable media identity is unavailable"))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use crate::source_lifecycle::{SourceLifecycleRegistry, SourceProvenance};

    use super::*;

    type TestRegistry = SourceLifecycleRegistry<RemovableMediaAdapter, Vec<Track>>;
    type TestConnectOwner =
        crate::source_lifecycle::ConnectOwner<RemovableMediaAdapter, Vec<Track>>;
    type LiveCancellation = (TestRegistry, TestConnectOwner, CancellationObserver);

    fn minimal_wav_bytes(sample: u8) -> Vec<u8> {
        let data_size = 1_u32;
        let mut bytes = Vec::with_capacity(45);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.push(sample);
        bytes
    }

    fn live_cancellation(source_id: SourceId) -> LiveCancellation {
        let registry = SourceLifecycleRegistry::new(Handle::current());
        registry
            .claim_provenance(source_id, SourceProvenance::Removable)
            .expect("claim removable source");
        let owner = registry.begin_connect(source_id).expect("begin scan");
        let cancellation = owner.cancellation();
        (registry, owner, cancellation)
    }

    fn read_stream(stream: AdapterStream) -> Vec<u8> {
        let AdapterStream::File(media) = stream else {
            panic!("fixture expected retained removable file");
        };
        let mut file = media.try_clone_file().expect("clone removable media");
        file.seek(SeekFrom::Start(0))
            .expect("rewind removable media");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read removable media");
        bytes
    }

    #[tokio::test]
    async fn scan_publishes_pathless_rows_and_resolves_only_accepted_identity() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let album = mount.path().join("album");
        std::fs::create_dir(&album).expect("create album");
        let accepted_path = album.join("song.wav");
        let accepted_bytes = minimal_wav_bytes(0x80);
        std::fs::write(&accepted_path, &accepted_bytes).expect("write accepted WAV");
        std::fs::write(mount.path().join("notes.txt"), b"not audio").expect("write non-audio file");

        let source_id = SourceId::removable("test:pathless-scan").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = RemovableMediaAdapter::scan(
            source_id,
            mount.path().to_path_buf(),
            &cancellation,
            Handle::current(),
        )
        .expect("scan removable media")
        .expect("scan remains current");
        assert_eq!(adapter.source_id(), source_id);
        assert_eq!(adapter.tracks().len(), 1);

        let track = &adapter.tracks()[0];
        assert!(track.file_path.is_none());
        assert!(track.stream_url.is_none());
        assert!(track.cover_art_url.is_none());
        let track_id = track.native_track_id.clone().expect("native identity");
        assert_eq!(
            track_id.removable_relative_path().expect("relative path"),
            PathBuf::from("album").join("song.wav")
        );

        let unseen_path = mount.path().join("appeared-later.wav");
        std::fs::write(&unseen_path, minimal_wav_bytes(0x40)).expect("write later WAV");
        let unseen_id = TrackId::removable_relative(mount.path(), &unseen_path)
            .expect("later relative identity");
        let adapter = Arc::new(adapter);
        let Err(unavailable) = Arc::clone(&adapter).resolve_stream(unseen_id).await else {
            panic!("an unaccepted path must not resolve");
        };
        assert_eq!(
            unavailable.to_string(),
            "Internal error: removable media identity is unavailable"
        );
        assert!(!unavailable
            .to_string()
            .contains(&mount.path().display().to_string()));
        let stream = Arc::clone(&adapter)
            .resolve_stream(track_id)
            .await
            .expect("resolve accepted media");
        assert_eq!(read_stream(stream), accepted_bytes);
        drop(owner);
    }

    #[tokio::test]
    async fn cancellation_is_not_reported_as_a_scan_failure() {
        let source_id = SourceId::removable("test:cancelled-scan").expect("source identity");
        let (registry, owner, cancellation) = live_cancellation(source_id);
        drop(owner);
        assert!(cancellation.is_cancelled());

        let missing = std::env::temp_dir().join("tributary-cancelled-removable-mount");
        assert!(
            RemovableMediaAdapter::scan(source_id, missing, &cancellation, Handle::current(),)
                .expect("cancelled scan is not a failure")
                .is_none()
        );
        drop(registry);
    }

    #[tokio::test]
    async fn scan_failure_does_not_format_the_native_mount_path() {
        let parent = tempfile::tempdir().expect("temporary mount parent");
        let missing = parent.path().join("private-native-mount");
        let source_id = SourceId::removable("test:pathless-failure").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let Err(error) = RemovableMediaAdapter::scan(
            source_id,
            missing.clone(),
            &cancellation,
            Handle::current(),
        ) else {
            panic!("a missing mount must fail");
        };
        assert_eq!(
            error.to_string(),
            "Internal error: removable media scan failed"
        );
        assert!(!error.to_string().contains(&missing.display().to_string()));
        drop(owner);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_not_catalogued() {
        use std::os::unix::fs::symlink;

        let mount = tempfile::tempdir().expect("temporary removable mount");
        let outside = tempfile::tempdir().expect("outside directory");
        std::fs::write(mount.path().join("inside.wav"), minimal_wav_bytes(0x80))
            .expect("write inside WAV");
        std::fs::write(outside.path().join("outside.wav"), minimal_wav_bytes(0x40))
            .expect("write outside WAV");
        symlink(outside.path(), mount.path().join("escape")).expect("link outside directory");
        symlink(
            outside.path().join("outside.wav"),
            mount.path().join("linked.wav"),
        )
        .expect("link outside file");

        let source_id = SourceId::removable("test:symlink-scan").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = RemovableMediaAdapter::scan(
            source_id,
            mount.path().to_path_buf(),
            &cancellation,
            Handle::current(),
        )
        .expect("scan removable media")
        .expect("scan remains current");
        assert_eq!(adapter.tracks().len(), 1);
        assert_eq!(
            adapter.tracks()[0]
                .native_track_id
                .as_ref()
                .expect("inside identity")
                .removable_relative_path()
                .expect("inside relative path"),
            PathBuf::from("inside.wav")
        );
        drop(owner);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn root_replacement_cannot_retarget_an_accepted_identity() {
        let parent = tempfile::tempdir().expect("mount parent");
        let mount_path = parent.path().join("mounted");
        let displaced_path = parent.path().join("displaced");
        std::fs::create_dir(&mount_path).expect("create mounted root");
        std::fs::write(mount_path.join("song.wav"), minimal_wav_bytes(0x80))
            .expect("write accepted WAV");

        let source_id = SourceId::removable("test:root-replacement").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = RemovableMediaAdapter::scan(
            source_id,
            mount_path.clone(),
            &cancellation,
            Handle::current(),
        )
        .expect("scan removable media")
        .expect("scan remains current");
        let track_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");

        std::fs::rename(&mount_path, &displaced_path).expect("displace mounted root");
        std::fs::create_dir(&mount_path).expect("create replacement root");
        std::fs::write(mount_path.join("song.wav"), minimal_wav_bytes(0x40))
            .expect("write replacement WAV");

        assert!(Arc::new(adapter).resolve_stream(track_id).await.is_err());
        drop(owner);
    }
}
