//! Lifecycle-owned adapter for one exact mounted removable filesystem.
//!
//! Construction walks the native mount only on a blocking worker, binds every
//! accepted file through [`MountedRootAuthority`], and publishes no playable
//! path or URI. The catalogue's opaque [`TrackId`] values losslessly encode
//! mount-relative native paths; resolution decodes only an accepted identity
//! and opens it again beneath the still-current retained mount authority.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    CatalogueFuture, ManagedSourceAdapter, MutationTargetFuture, PlaybackAttributionCapability,
    PlaybackAttributionProfile, StreamFuture,
};

const MAX_TAG_TEXT_BYTES: usize = 64 * 1024;

/// Accepted catalogue content of one mounted removable session.
///
/// Mutable only through [`RemovableMediaAdapter::refresh_catalogue_after_mutation`]:
/// a committed retained write swaps the refreshed entry and attribution
/// profile for exactly the written identity under one lock, so the
/// republished catalogue and the live attribution authority can never
/// disagree about a mutated row.
#[derive(Default)]
struct RemovableCatalogue {
    tracks: Vec<Track>,
    playback_attribution_profiles: HashMap<TrackId, PlaybackAttributionProfile>,
}

/// One mounted removable source whose catalogue and media authority share an
/// exact lifecycle generation.
pub struct RemovableMediaAdapter {
    source_id: SourceId,
    authority: Arc<MountedRootAuthority>,
    accepted_track_ids: HashSet<TrackId>,
    /// Accepted catalogue. Scan constructs it once; the post-mutation
    /// refresh replaces entries for exact written identities under the
    /// catalogue lock.
    catalogue: Mutex<RemovableCatalogue>,
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
        let mut playback_attribution_profiles = HashMap::new();
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
                let title_from_tag = parsed.title_from_tag;
                let artist_from_tag = parsed.artist_from_tag;
                let album_from_tag = parsed.album_from_tag;
                let track = pathless_track(source_id, track_id.clone(), parsed);
                if let Some(profile) = PlaybackAttributionProfile::from_tagged_track(
                    &track,
                    title_from_tag,
                    artist_from_tag,
                    album_from_tag,
                ) {
                    let replaced = playback_attribution_profiles.insert(track_id, profile);
                    debug_assert!(
                        replaced.is_none(),
                        "accepted removable identities are unique within one scan"
                    );
                }
                tracks.push(track);
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
            source_id,
            authority,
            accepted_track_ids,
            catalogue: Mutex::new(RemovableCatalogue {
                tracks,
                playback_attribution_profiles,
            }),
            runtime,
        }))
    }

    #[cfg(test)]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[cfg(test)]
    pub fn tracks(&self) -> Vec<Track> {
        self.catalogue_tracks()
    }

    /// Snapshot of the accepted catalogue tracks under the catalogue lock.
    fn catalogue_tracks(&self) -> Vec<Track> {
        self.catalogue
            .lock()
            .expect("removable catalogue mutex poisoned")
            .tracks
            .clone()
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

    fn playback_attribution_profile(
        &self,
        track_id: &TrackId,
    ) -> Option<PlaybackAttributionProfile> {
        self.catalogue
            .lock()
            .expect("removable catalogue mutex poisoned")
            .playback_attribution_profiles
            .get(track_id)
            .cloned()
    }

    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move { Ok(self.catalogue_tracks()) })
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

    fn resolve_mutation_target(self: Arc<Self>, track_id: TrackId) -> MutationTargetFuture {
        Box::pin(async move {
            // The same exact-membership gate as stream resolution: only an
            // identity the accepted scan admitted may become a write target,
            // and only through this session's retained mount authority.
            if !self.accepted_track_ids.contains(&track_id) {
                return Err(resolution_failed());
            }
            let relative_path = track_id
                .removable_relative_path()
                .map_err(|_| resolution_failed())?;
            let authority = Arc::clone(&self.authority);
            let task = self.runtime.spawn_blocking(move || {
                authority
                    .open_mutation_target(&relative_path)
                    .map_err(|_| resolution_failed())
            });

            task.await.map_err(|_| resolution_failed())?
        })
    }

    fn refresh_catalogue_after_mutation(
        self: Arc<Self>,
        written_track_ids: HashSet<TrackId>,
        cancellation: CancellationObserver,
    ) -> CatalogueFuture {
        Box::pin(async move {
            if written_track_ids.is_empty() {
                return Ok(self.catalogue_tracks());
            }
            let this = Arc::clone(&self);
            let task = self.runtime.spawn_blocking(move || {
                this.refresh_written_identities(&written_track_ids, &cancellation)
            });
            match task.await {
                Ok(result) => result,
                Err(_) => Err(scan_failed()),
            }
        })
    }
}

impl RemovableMediaAdapter {
    /// Re-derive catalogue entries for exact identities whose retained
    /// writes committed, and swap them into the accepted catalogue under
    /// one lock.
    ///
    /// Only identities the accepted scan admitted are re-read; every other
    /// requested identity is ignored (see [`Self::refreshed_identity`]).
    /// One unreadable row never fails the whole publication, and a lane
    /// cancelled mid-re-read stops re-reading and publishes nothing (see
    /// [`Self::publish_refreshed`]). Blocking — worker threads only.
    fn refresh_written_identities(
        &self,
        written: &HashSet<TrackId>,
        cancellation: &CancellationObserver,
    ) -> BackendResult<Vec<Track>> {
        let mut refreshed: Vec<(TrackId, Track, Option<PlaybackAttributionProfile>)> =
            Vec::with_capacity(written.len());
        for track_id in written {
            if cancellation.is_cancelled() {
                break;
            }
            if let Some(row) = self.refreshed_identity(track_id, cancellation) {
                refreshed.push(row);
            }
        }
        self.publish_refreshed(refreshed, cancellation)
    }

    /// Re-derive one accepted identity from the retained mount after its
    /// write committed. An identity that fails to re-read or re-parse —
    /// or one the accepted scan never admitted — yields `None` and keeps
    /// its previous catalogue entry: the refresh is scoped to what
    /// actually committed. Re-reads flow through the retained mount
    /// authority via the same [`ResolvedFileMedia::from_mounted_relative_path`]
    /// path the scan and stream resolution use, so no native mount location
    /// is exposed and a displaced ancestor can never retarget the lookup.
    /// The attribution profile is rebuilt from the fresh parse on the same
    /// real-tag provenance rules as the scan, so the republished catalogue
    /// and the live playback-attribution authority change together.
    fn refreshed_identity(
        &self,
        track_id: &TrackId,
        cancellation: &CancellationObserver,
    ) -> Option<(TrackId, Track, Option<PlaybackAttributionProfile>)> {
        // Membership: a well-formed relative identity that appeared
        // outside the accepted scan is not authority to read anything
        // in this session.
        if !self.accepted_track_ids.contains(track_id) {
            return None;
        }
        let relative_path = track_id.removable_relative_path().ok()?;
        let extension = extension_hint(&relative_path);
        let media = ResolvedFileMedia::from_mounted_relative_path(
            Arc::clone(&self.authority),
            &relative_path,
            extension,
        )
        .ok()?;
        let parsed = media
            .with_serialized_seekable_file(|file| {
                crate::local::tag_parser::parse_audio_file_from_file(file, &relative_path)
            })
            .ok()
            .and_then(Result::ok);
        if cancellation.is_cancelled() {
            return None;
        }
        let parsed = parsed.filter(parsed_metadata_is_bounded)?;
        let title_from_tag = parsed.title_from_tag;
        let artist_from_tag = parsed.artist_from_tag;
        let album_from_tag = parsed.album_from_tag;
        let track = pathless_track(self.source_id, track_id.clone(), parsed);
        let profile = PlaybackAttributionProfile::from_tagged_track(
            &track,
            title_from_tag,
            artist_from_tag,
            album_from_tag,
        );
        Some((track_id.clone(), track, profile))
    }

    /// Swap refreshed rows into the accepted catalogue under one lock.
    ///
    /// The swap happens only after the mount authority revalidates: a mount
    /// that changed under the re-reads publishes nothing. A lane cancelled
    /// mid-re-read publishes nothing — the same discipline as the accepted
    /// scan, which never repopulates from a cancelled pass.
    fn publish_refreshed(
        &self,
        refreshed: Vec<(TrackId, Track, Option<PlaybackAttributionProfile>)>,
        cancellation: &CancellationObserver,
    ) -> BackendResult<Vec<Track>> {
        if self.authority.validate().is_err() {
            if cancellation.is_cancelled() {
                return Ok(self.catalogue_tracks());
            }
            return Err(scan_failed());
        }
        if cancellation.is_cancelled() {
            return Ok(self.catalogue_tracks());
        }

        let mut catalogue = self
            .catalogue
            .lock()
            .expect("removable catalogue mutex poisoned");
        for (track_id, track, profile) in refreshed {
            if let Some(slot) = catalogue
                .tracks
                .iter_mut()
                .find(|track| track.native_track_id.as_ref() == Some(&track_id))
            {
                *slot = track;
            }
            match profile {
                Some(profile) => {
                    catalogue
                        .playback_attribution_profiles
                        .insert(track_id, profile);
                }
                None => {
                    catalogue.playback_attribution_profiles.remove(&track_id);
                }
            }
        }
        Ok(catalogue.tracks.clone())
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

    #[allow(clippy::too_many_arguments)]
    fn write_tagged_flac(
        path: &Path,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        album_artist: Option<&str>,
        track_number: Option<&str>,
    ) {
        std::fs::write(
            path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/audio/silence.flac"
            )),
        )
        .expect("copy removable FLAC fixture");
        crate::local::tag_writer::write_tags(
            path,
            &crate::local::tag_writer::TagEdits {
                // Explicit empty edits clear any fixture metadata and let the
                // parser prove absence rather than relying on fixture shape.
                title: Some(title.unwrap_or_default().to_string()),
                artist: Some(artist.unwrap_or_default().to_string()),
                album: Some(album.unwrap_or_default().to_string()),
                album_artist: Some(album_artist.unwrap_or_default().to_string()),
                track_number: Some(track_number.unwrap_or_default().to_string()),
                ..Default::default()
            },
        )
        .expect("write removable FLAC tags");
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
    async fn scan_captures_only_exact_real_tag_attribution_profiles() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let required_only_path = mount.path().join("required-only.flac");
        write_tagged_flac(
            &required_only_path,
            Some("Private Removable Title"),
            Some("Private Removable Artist"),
            None,
            Some("Private Album Artist"),
            Some("7"),
        );
        let tagged_album_path = mount.path().join("tagged-album.flac");
        write_tagged_flac(
            &tagged_album_path,
            Some("Album Track"),
            Some("Album Artist"),
            Some("Real Tagged Album"),
            None,
            Some("3"),
        );
        let missing_title_path = mount.path().join("missing-title.flac");
        write_tagged_flac(
            &missing_title_path,
            None,
            Some("Only Artist Is Tagged"),
            Some("Tagged Album"),
            None,
            None,
        );
        let missing_artist_path = mount.path().join("missing-artist.flac");
        write_tagged_flac(
            &missing_artist_path,
            Some("Only Title Is Tagged"),
            None,
            Some("Tagged Album"),
            None,
            None,
        );
        let untagged_path = mount.path().join("filename-fallback.wav");
        std::fs::write(&untagged_path, minimal_wav_bytes(0x80)).expect("write untagged WAV");

        let source_id = SourceId::removable("test:tagged-attribution").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = RemovableMediaAdapter::scan(
            source_id,
            mount.path().to_path_buf(),
            &cancellation,
            Handle::current(),
        )
        .expect("scan removable media")
        .expect("scan remains current");
        assert_eq!(adapter.tracks().len(), 5);

        let required_only_id =
            TrackId::removable_relative(mount.path(), &required_only_path).expect("required ID");
        let required_only = adapter
            .playback_attribution_profile(&required_only_id)
            .expect("real title and artist tags authorize attribution");
        assert_eq!(required_only.title(), "Private Removable Title");
        assert_eq!(required_only.artist(), "Private Removable Artist");
        assert_eq!(
            required_only.album(),
            None,
            "the display-only Unknown Album fallback is never authorized"
        );
        assert_eq!(required_only.album_artist(), Some("Private Album Artist"));
        assert_eq!(required_only.track_number(), Some(7));
        let published_duration = adapter
            .tracks()
            .iter()
            .find(|track| track.native_track_id.as_ref() == Some(&required_only_id))
            .and_then(|track| track.duration_secs);
        assert_eq!(required_only.duration_secs(), published_duration);
        let debug = format!("{required_only:?}");
        assert!(!debug.contains("Private Removable Title"));
        assert!(!debug.contains("Private Removable Artist"));

        let tagged_album_id =
            TrackId::removable_relative(mount.path(), &tagged_album_path).expect("album ID");
        let tagged_album = adapter
            .playback_attribution_profile(&tagged_album_id)
            .expect("tagged album track profile");
        assert_eq!(tagged_album.album(), Some("Real Tagged Album"));
        assert_eq!(tagged_album.track_number(), Some(3));

        for path in [&missing_title_path, &missing_artist_path, &untagged_path] {
            let track_id =
                TrackId::removable_relative(mount.path(), path).expect("ineligible track ID");
            assert!(
                adapter.playback_attribution_profile(&track_id).is_none(),
                "filename and Unknown fallbacks cannot replace either required tag"
            );
        }
        let unseen_id = TrackId::removable_relative(
            mount.path(),
            &mount.path().join("not-in-the-accepted-scan.flac"),
        )
        .expect("unseen lexical identity");
        assert!(adapter.playback_attribution_profile(&unseen_id).is_none());
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

    /// Scan a one-track mount and hand back the live adapter plus the
    /// accepted identity of its only row.
    fn scanned_single_track(
        source_id: SourceId,
        mount: &Path,
        cancellation: &CancellationObserver,
    ) -> (Arc<RemovableMediaAdapter>, TrackId) {
        let adapter = Arc::new(
            RemovableMediaAdapter::scan(
                source_id,
                mount.to_path_buf(),
                cancellation,
                Handle::current(),
            )
            .expect("scan removable media")
            .expect("scan remains current"),
        );
        assert_eq!(adapter.tracks().len(), 1);
        let track_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");
        (adapter, track_id)
    }

    /// A committed retained write replaced the file content at the same
    /// relative path; the refresh must republish the row and the
    /// attribution profile from the fresh parse.
    fn assert_republished_attribution(adapter: &RemovableMediaAdapter, track_id: &TrackId) {
        let published = adapter.tracks();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].title, "After Refresh");
        assert_eq!(
            published[0].native_track_id.as_ref(),
            Some(track_id),
            "the row keeps its accepted identity"
        );
        let profile = adapter
            .playback_attribution_profile(track_id)
            .expect("fresh real tags keep attribution authorized");
        assert_eq!(profile.title(), "After Refresh");
        assert_eq!(profile.track_number(), Some(2));
    }

    #[tokio::test]
    async fn refresh_republishes_written_identities_and_attribution() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let song_path = mount.path().join("song.flac");
        write_tagged_flac(
            &song_path,
            Some("Before Refresh"),
            Some("Before Artist"),
            Some("Before Album"),
            None,
            Some("1"),
        );

        let source_id = SourceId::removable("test:refresh-republish").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let (adapter, track_id) = scanned_single_track(source_id, mount.path(), &cancellation);
        assert_eq!(adapter.tracks()[0].title, "Before Refresh");

        write_tagged_flac(
            &song_path,
            Some("After Refresh"),
            Some("After Artist"),
            Some("After Album"),
            None,
            Some("2"),
        );
        let refreshed = Arc::clone(&adapter)
            .refresh_catalogue_after_mutation(
                HashSet::from([track_id.clone()]),
                cancellation.clone(),
            )
            .await
            .expect("a committed write republishes");
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].title, "After Refresh");

        assert_republished_attribution(&adapter, &track_id);
        drop(owner);
    }

    #[tokio::test]
    async fn refresh_ignores_identities_outside_the_accepted_scan() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let accepted_path = mount.path().join("accepted.wav");
        std::fs::write(&accepted_path, minimal_wav_bytes(0x80)).expect("write accepted WAV");

        let source_id = SourceId::removable("test:refresh-membership").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = Arc::new(
            RemovableMediaAdapter::scan(
                source_id,
                mount.path().to_path_buf(),
                &cancellation,
                Handle::current(),
            )
            .expect("scan removable media")
            .expect("scan remains current"),
        );
        let accepted_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");

        let appeared_path = mount.path().join("appeared-later.wav");
        std::fs::write(&appeared_path, minimal_wav_bytes(0x40)).expect("write later WAV");
        let appeared_id =
            TrackId::removable_relative(mount.path(), &appeared_path).expect("later identity");

        let refreshed = Arc::clone(&adapter)
            .refresh_catalogue_after_mutation(
                HashSet::from([accepted_id.clone(), appeared_id]),
                cancellation.clone(),
            )
            .await
            .expect("unaccepted identities are ignored, never a failure");
        assert_eq!(
            refreshed.len(),
            1,
            "an identity outside the accepted scan is never catalogued"
        );
        assert_eq!(refreshed[0].native_track_id.as_ref(), Some(&accepted_id));
        assert_eq!(adapter.tracks().len(), 1);
        drop(owner);
    }

    #[tokio::test]
    async fn refresh_failed_reread_keeps_the_previous_catalogue_entry() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let song_path = mount.path().join("song.flac");
        write_tagged_flac(
            &song_path,
            Some("Kept Title"),
            Some("Kept Artist"),
            None,
            None,
            None,
        );

        let source_id =
            SourceId::removable("test:refresh-reread-failure").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = Arc::new(
            RemovableMediaAdapter::scan(
                source_id,
                mount.path().to_path_buf(),
                &cancellation,
                Handle::current(),
            )
            .expect("scan removable media")
            .expect("scan remains current"),
        );
        let track_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");

        std::fs::remove_file(&song_path).expect("remove the written file");
        let refreshed = Arc::clone(&adapter)
            .refresh_catalogue_after_mutation(
                HashSet::from([track_id.clone()]),
                cancellation.clone(),
            )
            .await
            .expect("one unreadable row never fails the whole refresh");
        assert_eq!(
            refreshed[0].title, "Kept Title",
            "an identity that fails to re-read keeps its previous entry"
        );
        assert!(adapter.playback_attribution_profile(&track_id).is_some());
        drop(owner);
    }

    #[tokio::test]
    async fn refresh_publishes_nothing_when_the_mount_changed_under_the_rereads() {
        let parent = tempfile::tempdir().expect("mount parent");
        let mount_path = parent.path().join("mounted");
        let displaced_path = parent.path().join("displaced");
        std::fs::create_dir(&mount_path).expect("create mounted root");
        std::fs::write(mount_path.join("song.wav"), minimal_wav_bytes(0x80))
            .expect("write accepted WAV");

        let source_id = SourceId::removable("test:refresh-mount-change").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = Arc::new(
            RemovableMediaAdapter::scan(
                source_id,
                mount_path.clone(),
                &cancellation,
                Handle::current(),
            )
            .expect("scan removable media")
            .expect("scan remains current"),
        );
        let track_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");
        let title_before = adapter.tracks()[0].title.clone();

        std::fs::rename(&mount_path, &displaced_path).expect("displace mounted root");
        std::fs::create_dir(&mount_path).expect("create replacement root");
        std::fs::write(mount_path.join("song.wav"), minimal_wav_bytes(0x40))
            .expect("write replacement WAV");

        let error = Arc::clone(&adapter)
            .refresh_catalogue_after_mutation(
                HashSet::from([track_id.clone()]),
                cancellation.clone(),
            )
            .await
            .expect_err("a mount that changed under the re-reads publishes nothing");
        assert_eq!(
            error.to_string(),
            "Internal error: removable media scan failed"
        );
        assert!(!error
            .to_string()
            .contains(&mount_path.display().to_string()));
        let published = adapter.tracks();
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].title, title_before,
            "the accepted entry is not displaced by the replacement root"
        );
        drop(owner);
    }

    #[tokio::test]
    async fn cancelled_refresh_is_not_a_failure_and_publishes_nothing() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let song_path = mount.path().join("song.flac");
        write_tagged_flac(
            &song_path,
            Some("Before Cancel"),
            Some("Cancel Artist"),
            None,
            None,
            None,
        );

        let source_id = SourceId::removable("test:refresh-cancelled").expect("source identity");
        let (registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = Arc::new(
            RemovableMediaAdapter::scan(
                source_id,
                mount.path().to_path_buf(),
                &cancellation,
                Handle::current(),
            )
            .expect("scan removable media")
            .expect("scan remains current"),
        );
        let track_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");
        drop(owner);
        assert!(cancellation.is_cancelled());

        // Even a write that committed before the lane was cancelled must not
        // republish: a cancelled lane publishes nothing.
        write_tagged_flac(
            &song_path,
            Some("After Cancel"),
            Some("Cancel Artist"),
            None,
            None,
            None,
        );
        let refreshed = Arc::clone(&adapter)
            .refresh_catalogue_after_mutation(
                HashSet::from([track_id.clone()]),
                cancellation.clone(),
            )
            .await
            .expect("cancelled refresh is not a failure");
        assert_eq!(refreshed[0].title, "Before Cancel");
        assert_eq!(adapter.tracks()[0].title, "Before Cancel");
        drop(registry);
    }

    #[tokio::test]
    async fn refresh_drops_attribution_when_the_fresh_parse_loses_real_tags() {
        let mount = tempfile::tempdir().expect("temporary removable mount");
        let song_path = mount.path().join("song.flac");
        write_tagged_flac(
            &song_path,
            Some("Tagged Title"),
            Some("Tagged Artist"),
            None,
            None,
            None,
        );

        let source_id =
            SourceId::removable("test:refresh-attribution-loss").expect("source identity");
        let (_registry, owner, cancellation) = live_cancellation(source_id);
        let adapter = Arc::new(
            RemovableMediaAdapter::scan(
                source_id,
                mount.path().to_path_buf(),
                &cancellation,
                Handle::current(),
            )
            .expect("scan removable media")
            .expect("scan remains current"),
        );
        let track_id = adapter.tracks()[0]
            .native_track_id
            .clone()
            .expect("accepted identity");
        assert!(adapter.playback_attribution_profile(&track_id).is_some());

        // A committed write cleared the real tags: the stale profile must not
        // survive the refresh, because attribution authority follows the
        // fresh parse.
        write_tagged_flac(&song_path, None, None, None, None, None);
        let refreshed = Arc::clone(&adapter)
            .refresh_catalogue_after_mutation(
                HashSet::from([track_id.clone()]),
                cancellation.clone(),
            )
            .await
            .expect("the refresh republishes the cleared row");
        assert_eq!(refreshed.len(), 1);
        assert!(
            adapter.playback_attribution_profile(&track_id).is_none(),
            "a profile the fresh parse no longer supports is dropped"
        );
        assert_eq!(adapter.tracks().len(), 1);
        drop(owner);
    }
}
