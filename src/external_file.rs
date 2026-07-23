//! Ephemeral lifecycle adapter for media delivered by the operating system.
//!
//! Admission starts from an already-open file object. The exact retained
//! object is parsed before random source/track identity is minted; no path or
//! URI is retained in the adapter, accepted catalogue, or resolved stream.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use uuid::Uuid;

use crate::architecture::backend::BackendResult;
use crate::architecture::error::BackendError;
use crate::architecture::models::Track;
use crate::architecture::{SourceId, TrackId};
use crate::local::resolver::ResolvedFileMedia;
use crate::source_lifecycle::{
    AdapterCloseFuture, AdapterStream, CloseAuthority, LifecycleAdapter,
};
use crate::source_registry::{
    CatalogueFuture, ManagedSourceAdapter, PlaybackAttributionCapability,
    PlaybackAttributionProfile, StreamFuture,
};

const MAX_DISPLAY_HINT_BYTES: usize = 4 * 1024;
const MAX_EXTENSION_HINT_BYTES: usize = 16;
const MAX_TAG_TEXT_BYTES: usize = 64 * 1024;

/// Bounded, non-authoritative presentation hints for one OS-opened file.
///
/// The display name must be a single filename rather than a pathname or URI.
/// It exists only to select a parser format and provide a title when the file
/// has no title tag; the adapter never retains it after admission.
pub struct ExternalFileHint {
    display_name: String,
    extension: Option<String>,
}

impl ExternalFileHint {
    pub fn new(display_name: impl Into<String>, extension: Option<&str>) -> BackendResult<Self> {
        let display_name = display_name.into();
        if display_name.is_empty()
            || display_name.len() > MAX_DISPLAY_HINT_BYTES
            || display_name
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n' | '/' | '\\' | ':'))
            || matches!(display_name.as_str(), "." | "..")
        {
            return Err(closed_validation_error());
        }

        let extension = extension
            .map(|value| {
                let normalized = value.to_ascii_lowercase();
                if normalized.is_empty()
                    || normalized.len() > MAX_EXTENSION_HINT_BYTES
                    || !normalized.bytes().all(|byte| byte.is_ascii_alphanumeric())
                {
                    return Err(closed_validation_error());
                }
                Ok(normalized)
            })
            .transpose()?;

        Ok(Self {
            display_name,
            extension,
        })
    }

    fn parser_hint(&self) -> PathBuf {
        let mut hint = PathBuf::from(&self.display_name);
        if let Some(extension) = &self.extension {
            hint.set_extension(extension);
        }
        hint
    }
}

impl std::fmt::Debug for ExternalFileHint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalFileHint")
            .field("has_extension", &self.extension.is_some())
            .finish_non_exhaustive()
    }
}

/// One validated, pathless external-file adapter.
pub struct ExternalFileAdapter {
    source_id: SourceId,
    track_id: TrackId,
    track: Track,
    playback_attribution_profile: Option<PlaybackAttributionProfile>,
    media: ResolvedFileMedia,
    close_calls: Arc<AtomicUsize>,
}

/// Parsed exact-file authority which is not yet a lifecycle adapter and owns
/// no source or track identity. It may be dropped if shutdown closes admission
/// while blocking validation is in flight.
pub struct ExternalFileCandidate {
    media: ResolvedFileMedia,
    parsed: crate::local::tag_parser::ParsedTrack,
}

impl ExternalFileCandidate {
    /// Validate and parse the exact retained object without assigning identity
    /// or constructing a lifecycle adapter.
    pub fn validate(file: File, hint: ExternalFileHint) -> BackendResult<Self> {
        let media = ResolvedFileMedia::from_open_regular_file(file, hint.extension.clone())
            .map_err(|_| closed_validation_error())?;
        let parser_hint = hint.parser_hint();
        let parsed = media
            .with_serialized_seekable_file(|parse_file| {
                crate::local::tag_parser::parse_audio_file_from_file(parse_file, &parser_hint)
            })
            .map_err(|_| closed_validation_error())?
            .map_err(|_| closed_validation_error())?;
        validate_parsed_metadata(&parsed)?;

        Ok(Self { media, parsed })
    }

    /// Mint identity and create the adapter only while the registry's
    /// publication/shutdown gate proves admission remains open.
    pub fn into_adapter(self) -> ExternalFileAdapter {
        // Identity is deliberately minted only after exact-handle parsing has
        // established that the candidate is accepted audio.
        let source_id = SourceId::external();
        let track_id = TrackId::external();
        let compatibility_id = Uuid::parse_str(track_id.as_str())
            .expect("external track identity is minted from UUIDv4");
        let parsed = self.parsed;
        let title_from_tag = parsed.title_from_tag;
        let artist_from_tag = parsed.artist_from_tag;
        let album_from_tag = parsed.album_from_tag;
        let track = Track {
            id: compatibility_id,
            native_track_id: Some(track_id.clone()),
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
        };
        let playback_attribution_profile = PlaybackAttributionProfile::from_tagged_track(
            &track,
            title_from_tag,
            artist_from_tag,
            album_from_tag,
        );

        ExternalFileAdapter {
            source_id,
            track_id,
            track,
            playback_attribution_profile,
            media: self.media,
            close_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ExternalFileAdapter {
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub fn track(&self) -> &Track {
        &self.track
    }

    #[cfg(test)]
    pub fn close_probe(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.close_calls)
    }
}

impl LifecycleAdapter for ExternalFileAdapter {
    fn close(self: Arc<Self>, _authority: CloseAuthority) -> AdapterCloseFuture {
        self.close_calls.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }
}

impl ManagedSourceAdapter for ExternalFileAdapter {
    fn playback_attribution_capability(&self) -> PlaybackAttributionCapability {
        PlaybackAttributionCapability::External
    }

    fn playback_attribution_profile(
        &self,
        track_id: &TrackId,
    ) -> Option<PlaybackAttributionProfile> {
        (track_id == &self.track_id)
            .then(|| self.playback_attribution_profile.clone())
            .flatten()
    }

    fn load_initial_catalogue(self: Arc<Self>) -> CatalogueFuture {
        Box::pin(async move { Ok(vec![self.track.clone()]) })
    }

    fn resolve_stream(self: Arc<Self>, track_id: TrackId) -> StreamFuture {
        Box::pin(async move {
            if track_id != self.track_id {
                return Err(closed_unavailable_error());
            }
            Ok(AdapterStream::File(self.media.clone()))
        })
    }
}

fn validate_parsed_metadata(parsed: &crate::local::tag_parser::ParsedTrack) -> BackendResult<()> {
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
    if required
        .into_iter()
        .any(|value| value.is_empty() || value.len() > MAX_TAG_TEXT_BYTES)
        || optional
            .into_iter()
            .flatten()
            .any(|value| value.len() > MAX_TAG_TEXT_BYTES)
    {
        return Err(closed_validation_error());
    }
    Ok(())
}

fn closed_validation_error() -> BackendError {
    BackendError::Internal(anyhow::anyhow!("external media validation failed"))
}

fn closed_unavailable_error() -> BackendError {
    BackendError::Internal(anyhow::anyhow!("external media identity is unavailable"))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use super::*;

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

    fn read_media(media: &ResolvedFileMedia) -> Vec<u8> {
        let mut file = media.try_clone_file().expect("clone retained file");
        file.seek(SeekFrom::Start(0)).expect("rewind retained file");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read retained file");
        bytes
    }

    #[test]
    fn hints_are_bounded_and_cannot_carry_a_path_or_uri() {
        assert!(ExternalFileHint::new("song.wav", Some("wav")).is_ok());
        assert!(ExternalFileHint::new("/tmp/song.wav", Some("wav")).is_err());
        assert!(ExternalFileHint::new("C:\\music\\song.wav", Some("wav")).is_err());
        assert!(ExternalFileHint::new("file:song.wav", Some("wav")).is_err());
        assert!(ExternalFileHint::new("song.wav", Some("bad/ext")).is_err());
    }

    #[test]
    fn admitted_adapter_retains_original_object_after_path_replacement() {
        let directory = tempfile::tempdir().expect("external media directory");
        let path = directory.path().join("song.wav");
        let displaced = directory.path().join("original.wav");
        let original = minimal_wav_bytes(128);
        let replacement = minimal_wav_bytes(64);
        std::fs::write(&path, &original).expect("write original WAV");
        let candidate = ExternalFileCandidate::validate(
            File::open(&path).expect("open original WAV"),
            ExternalFileHint::new("song.wav", Some("wav")).expect("safe hint"),
        )
        .expect("validate original WAV");
        assert!(!candidate.parsed.title_from_tag);
        assert_eq!(candidate.parsed.artist_name, "Unknown Artist");
        assert!(!candidate.parsed.artist_from_tag);
        assert_eq!(candidate.parsed.album_title, "Unknown Album");
        assert!(!candidate.parsed.album_from_tag);

        let replaced = match std::fs::rename(&path, &displaced) {
            Ok(()) => {
                std::fs::write(&path, &replacement).expect("write replacement WAV");
                true
            }
            Err(error) => {
                #[cfg(not(windows))]
                panic!("replace admitted external path: {error}");
                #[cfg(windows)]
                {
                    let _ = error;
                    false
                }
            }
        };

        assert_eq!(read_media(&candidate.media), original);
        if replaced {
            assert_ne!(read_media(&candidate.media), replacement);
        }
        let adapter = candidate.into_adapter();
        assert!(adapter
            .playback_attribution_profile(adapter.track_id())
            .is_none());
        assert!(adapter.track.file_path.is_none());
        assert!(adapter.track.stream_url.is_none());
        assert!(adapter.track.cover_art_url.is_none());
        assert!(!format!("{:?}", adapter.track_id()).contains("song.wav"));
    }
}
