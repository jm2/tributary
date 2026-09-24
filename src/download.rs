//! Offline downloads of remote server tracks into an ordinary library folder.
//!
//! A download resolves its track through the source registry exactly as
//! playback does, so authentication, session leases, and credential isolation
//! are unchanged. The body is written to `<name>.<ext>.part`, synced, and then
//! renamed into place; the library scanner and watcher never index a `.part`
//! file. The result is an ordinary local copy with no link back to the remote
//! row.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::architecture::media::{MediaContainer, MediaRequest, ResolvedHttpRequest};
use crate::architecture::{SourceId, TrackId};
use crate::audio::cast_http_server::UpstreamMediaClient;
use crate::http_body::read_limited;
use crate::local::tag_parser::AUDIO_EXTENSIONS;
use crate::source_registry::{ResolvedSourceStream, SourceRegistry, StreamResolutionClass};

/// Folder created under the platform music directory when none is configured.
pub const DEFAULT_FOLDER_NAME: &str = "Tributary Downloads";

/// Tracks downloaded at the same time.
const CONCURRENT_DOWNLOADS: usize = 2;

/// Largest accepted file, so a misbehaving server cannot fill the disk.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest non-audio body read to look for a Subsonic refusal; an error
/// envelope is a few hundred bytes.
const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;

/// Longest file or folder name written, in bytes. Most filesystems allow 255;
/// the margin leaves room for the extension and `.part` suffix.
const MAX_NAME_BYTES: usize = 120;

/// `<platform music directory>/Tributary Downloads`, falling back to `~/Music`
/// like the default library folder.
pub fn default_download_dir() -> Option<PathBuf> {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Music")))
        .map(|music| music.join(DEFAULT_FOLDER_NAME))
}

/// The tags a downloaded file is named by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackNaming {
    pub album_artist: String,
    pub artist: String,
    pub album: String,
    pub title: String,
    pub disc_number: u32,
    pub track_number: u32,
}

impl TrackNaming {
    /// `<Album Artist or Artist>/<Album>/<NN Title>`, without an extension.
    ///
    /// A disc number above one prefixes the track number, so a title repeated
    /// on another disc cannot collide with (and be skipped as) the first.
    pub fn relative_stem(&self) -> PathBuf {
        let artist = if self.album_artist.trim().is_empty() {
            &self.artist
        } else {
            &self.album_artist
        };
        let name = match (self.disc_number, self.track_number) {
            (_, 0) => self.title.clone(),
            (0 | 1, track) => format!("{track:02} {}", self.title),
            (disc, track) => format!("{disc}-{track:02} {}", self.title),
        };
        [
            sanitize_name(artist, "Unknown Artist"),
            sanitize_name(&self.album, "Unknown Album"),
            sanitize_name(&name, "Untitled"),
        ]
        .iter()
        .collect()
    }
}

/// One file or folder name that is valid on Linux, macOS, and Windows.
///
/// Path separators, characters Windows reserves, and control characters
/// become `_`; a leading dot (hidden file) becomes `_`; trailing dots and
/// spaces, which Windows drops, are trimmed; Windows device names get a `_`
/// prefix; and the name is cut to [`MAX_NAME_BYTES`]. An empty result is
/// replaced by `fallback`.
pub fn sanitize_name(name: &str, fallback: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    if clean.len() > MAX_NAME_BYTES {
        let mut end = MAX_NAME_BYTES;
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
    }
    let trimmed = clean.trim_start().trim_end_matches(['.', ' ']).trim_end();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let mut clean = trimmed.to_string();
    if clean.starts_with('.') {
        clean.replace_range(..1, "_");
    }
    let device_stem = clean
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_device = matches!(device_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device_stem.len() == 4
            && (device_stem.starts_with("COM") || device_stem.starts_with("LPT"))
            && device_stem.as_bytes()[3].is_ascii_digit());
    if is_device {
        clean.insert(0, '_');
    }
    clean
}

/// One remote track to download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadItem {
    pub source_id: SourceId,
    pub session_epoch: u64,
    pub track_id: TrackId,
    /// Absolute destination without an extension; see
    /// [`TrackNaming::relative_stem`].
    pub stem: PathBuf,
}

/// What happened to one [`DownloadItem`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadOutcome {
    Downloaded,
    /// A file with the same name and any audio extension already exists.
    Skipped,
    Failed,
    /// The server refused the download: HTTP 401 or 403, or a Subsonic
    /// authorization error.
    Refused,
    Cancelled,
}

/// Download queued items [`CONCURRENT_DOWNLOADS`] at a time until the queue
/// closes, reporting each outcome with its item's source as it completes.
/// Items still queued after `cancel` fires report
/// [`DownloadOutcome::Cancelled`] without any I/O.
pub async fn run_queue(
    registry: SourceRegistry,
    http: UpstreamMediaClient,
    queue: async_channel::Receiver<DownloadItem>,
    cancel: CancellationToken,
    outcomes: async_channel::Sender<(SourceId, DownloadOutcome)>,
) {
    let workers = (0..CONCURRENT_DOWNLOADS).map(|_| {
        let (registry, http, queue, cancel, outcomes) = (
            registry.clone(),
            http.clone(),
            queue.clone(),
            cancel.clone(),
            outcomes.clone(),
        );
        async move {
            while let Ok(item) = queue.recv().await {
                let source_id = item.source_id;
                let outcome = download_track(&registry, &http, item, &cancel).await;
                if outcomes.send((source_id, outcome)).await.is_err() {
                    break;
                }
            }
        }
    });
    futures::future::join_all(workers).await;
}

async fn download_track(
    registry: &SourceRegistry,
    http: &UpstreamMediaClient,
    item: DownloadItem,
    cancel: &CancellationToken,
) -> DownloadOutcome {
    if cancel.is_cancelled() {
        return DownloadOutcome::Cancelled;
    }
    if existing_download(&item.stem).await {
        return DownloadOutcome::Skipped;
    }
    let resolved = tokio::select! {
        () = cancel.cancelled() => return DownloadOutcome::Cancelled,
        resolved = registry.resolve_stream_classified(
            item.source_id,
            item.session_epoch,
            item.track_id,
            StreamResolutionClass::Download,
        ) => resolved,
    };
    let request = match resolved {
        Ok(ResolvedSourceStream::Http(MediaRequest::ProtectedHttp(request))) => *request,
        Ok(_) => {
            warn!("Download refused: the track is not a remote server file");
            return DownloadOutcome::Failed;
        }
        Err(error) => {
            warn!(error = %error, "Could not resolve a track for download");
            return DownloadOutcome::Failed;
        }
    };
    fetch_outcome(fetch_to_file(http, request, &item.stem, cancel).await)
}

fn fetch_outcome(fetched: Result<PathBuf, FetchError>) -> DownloadOutcome {
    match fetched {
        Ok(_) => DownloadOutcome::Downloaded,
        Err(FetchError::Cancelled) => DownloadOutcome::Cancelled,
        Err(error) => {
            warn!(?error, "Track download failed");
            if error == FetchError::Refused {
                DownloadOutcome::Refused
            } else {
                DownloadOutcome::Failed
            }
        }
    }
}

/// Whether `stem` plus any indexable audio extension already exists, so a
/// track the server delivered in another container is still recognized.
async fn existing_download(stem: &Path) -> bool {
    for extension in AUDIO_EXTENSIONS {
        if tokio::fs::try_exists(with_suffix(stem, extension))
            .await
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// `path` with `.suffix` appended; unlike `Path::with_extension` this keeps a
/// dot already in the name ("01 Mr. Brightside").
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Why one fetch failed. Variants carry no request data, so they are safe to
/// log.
#[derive(Debug, PartialEq, Eq)]
enum FetchError {
    /// Connection, deadline, or body failure. The reqwest error is dropped
    /// because it can display the credential-bearing URL.
    Transport,
    /// A non-success HTTP status other than a refusal.
    Status(u16),
    /// The server refused the request: HTTP 401 or 403, or a Subsonic
    /// authorization error body. For Subsonic the cause can be the account's
    /// download permission, which `download.view` is subject to.
    Refused,
    /// The response is not audio, for example an HTML error page.
    NotAudio,
    /// Audio of a container the library cannot index.
    UnknownContainer,
    TooLarge,
    Empty,
    Io(std::io::ErrorKind),
    Cancelled,
}

/// Fetch one resolved request into `<stem>.<ext>`, where the extension comes
/// from the response. Every failure, including cancellation, removes the
/// `.part` file; a `.part` left by a crash is ignored by the library and
/// replaced by the next attempt.
async fn fetch_to_file(
    http: &UpstreamMediaClient,
    request: ResolvedHttpRequest,
    stem: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf, FetchError> {
    let resolved_container = request
        .representation()
        .ticket_suffix()
        .and_then(MediaContainer::from_suffix);
    let mut response = tokio::select! {
        () = cancel.cancelled() => return Err(FetchError::Cancelled),
        response = http.fetch(request) => response.ok_or(FetchError::Transport)?,
    };
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(FetchError::Refused);
    }
    if !status.is_success() {
        return Err(FetchError::Status(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FILE_BYTES)
    {
        return Err(FetchError::TooLarge);
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let container = match download_container(content_type, resolved_container) {
        // Subsonic reports errors in a small JSON body sent with status 200.
        Err(FetchError::NotAudio) => {
            let deadline = http.body_idle_timeout();
            let body = tokio::select! {
                () = cancel.cancelled() => return Err(FetchError::Cancelled),
                body = read_limited(response, MAX_ERROR_BODY_BYTES, deadline) => body,
            };
            return Err(match body {
                Ok(body) if crate::subsonic::is_refusal_body(&body) => FetchError::Refused,
                _ => FetchError::NotAudio,
            });
        }
        container => container?,
    };
    let target = with_suffix(stem, file_extension(container));
    let part = with_suffix(&target, "part");
    if let Some(folder) = target.parent() {
        tokio::fs::create_dir_all(folder).await.map_err(io_error)?;
    }

    let mut file = tokio::fs::File::create(&part).await.map_err(io_error)?;
    let written = write_body(&mut file, &mut response, http, cancel).await;
    // Flushing waits for tokio's background write, so the handle is really
    // closed before the rename or removal (Windows refuses both otherwise).
    let synced = match written {
        Ok(()) => match file.flush().await {
            Ok(()) => file.sync_all().await.map_err(io_error),
            Err(error) => Err(io_error(error)),
        },
        Err(error) => {
            let _ = file.flush().await;
            Err(error)
        }
    };
    drop(file);
    let renamed = match synced {
        Ok(()) => tokio::fs::rename(&part, &target).await.map_err(io_error),
        Err(error) => Err(error),
    };
    if let Err(error) = renamed {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(error);
    }
    Ok(target)
}

async fn write_body(
    file: &mut tokio::fs::File,
    response: &mut reqwest::Response,
    http: &UpstreamMediaClient,
    cancel: &CancellationToken,
) -> Result<(), FetchError> {
    let mut written = 0_u64;
    loop {
        let chunk = tokio::select! {
            () = cancel.cancelled() => return Err(FetchError::Cancelled),
            chunk = tokio::time::timeout(http.body_idle_timeout(), response.chunk()) => chunk,
        };
        let chunk = match chunk {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) if written == 0 => return Err(FetchError::Empty),
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(_)) | Err(_) => return Err(FetchError::Transport),
        };
        written = written.saturating_add(chunk.len() as u64);
        if written > MAX_FILE_BYTES {
            return Err(FetchError::TooLarge);
        }
        file.write_all(&chunk).await.map_err(io_error)?;
    }
}

fn io_error(error: std::io::Error) -> FetchError {
    FetchError::Io(error.kind())
}

/// The container a response is saved as.
///
/// An audio `Content-Type` is the wire truth. A missing or generic binary
/// type falls back to the container the resolver expects. Any other type
/// (an HTML error page, a Subsonic JSON error sent with status 200) is not
/// audio and is refused.
fn download_container(
    content_type: Option<&str>,
    resolved: Option<MediaContainer>,
) -> Result<MediaContainer, FetchError> {
    let mime = content_type
        .and_then(|value| value.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
        .filter(|mime| !mime.is_empty() && mime != "application/octet-stream");
    let Some(mime) = mime else {
        return resolved.ok_or(FetchError::UnknownContainer);
    };
    let Some(wire) = container_for_mime(&mime) else {
        return if mime.starts_with("audio/") {
            resolved.ok_or(FetchError::UnknownContainer)
        } else {
            Err(FetchError::NotAudio)
        };
    };
    // `audio/ogg` covers Ogg, Oga, and Opus: keep the resolver's more exact
    // container when it agrees with the wire type.
    Ok(resolved
        .filter(|container| container.content_type() == wire.content_type())
        .unwrap_or(wire))
}

fn container_for_mime(mime: &str) -> Option<MediaContainer> {
    Some(match mime {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => MediaContainer::Mp3,
        "audio/flac" | "audio/x-flac" => MediaContainer::Flac,
        "audio/ogg" | "audio/x-ogg" | "audio/vorbis" | "application/ogg" => MediaContainer::Ogg,
        "audio/opus" => MediaContainer::Opus,
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => MediaContainer::Wav,
        "audio/aac" | "audio/aacp" | "audio/x-aac" => MediaContainer::Aac,
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => MediaContainer::M4a,
        "audio/aiff" | "audio/x-aiff" => MediaContainer::Aiff,
        "audio/x-ms-wma" => MediaContainer::Wma,
        _ => return None,
    })
}

/// The extension a container is saved with; always one the library indexes.
const fn file_extension(container: MediaContainer) -> &'static str {
    match container {
        MediaContainer::Mp3 => "mp3",
        MediaContainer::Flac => "flac",
        MediaContainer::Ogg | MediaContainer::Oga => "ogg",
        MediaContainer::Opus => "opus",
        MediaContainer::Wav => "wav",
        MediaContainer::Aac => "aac",
        MediaContainer::M4a => "m4a",
        MediaContainer::Aiff => "aiff",
        MediaContainer::Wma => "wma",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{header, HeaderValue, StatusCode};

    use super::*;
    use crate::architecture::MediaRepresentation;
    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};
    use crate::subsonic::SubsonicClient;

    const AUDIO: &str = "fLaC-not-really-audio";

    fn audio_response() -> MockResponse {
        MockResponse::text(AUDIO)
            .with_header(header::CONTENT_TYPE, HeaderValue::from_static("audio/flac"))
    }

    fn naming(album_artist: &str, artist: &str, disc: u32, track: u32) -> TrackNaming {
        TrackNaming {
            album_artist: album_artist.to_string(),
            artist: artist.to_string(),
            album: "Album".to_string(),
            title: "Title".to_string(),
            disc_number: disc,
            track_number: track,
        }
    }

    #[test]
    fn layout_uses_album_artist_then_artist_and_numbers_tracks() {
        assert_eq!(
            naming("Various", "Solo", 1, 3).relative_stem(),
            Path::new("Various/Album/03 Title")
        );
        assert_eq!(
            naming(" ", "Solo", 0, 12).relative_stem(),
            Path::new("Solo/Album/12 Title")
        );
        assert_eq!(
            naming("", "Solo", 2, 1).relative_stem(),
            Path::new("Solo/Album/2-01 Title")
        );
        assert_eq!(
            naming("", "", 0, 0).relative_stem(),
            Path::new("Unknown Artist/Album/Title")
        );
    }

    #[test]
    fn names_are_sanitized_for_every_platform() {
        assert_eq!(sanitize_name("AC/DC", "x"), "AC_DC");
        assert_eq!(
            sanitize_name(r#"a\b:c*d?e"f<g>h|i"#, "x"),
            "a_b_c_d_e_f_g_h_i"
        );
        assert_eq!(sanitize_name("tab\there\n", "x"), "tab_here_");
        assert_eq!(sanitize_name("..", "Fallback"), "Fallback");
        assert_eq!(sanitize_name("  ", "Fallback"), "Fallback");
        assert_eq!(sanitize_name(".hidden", "x"), "_hidden");
        assert_eq!(sanitize_name("Trailing. . ", "x"), "Trailing");
        assert_eq!(sanitize_name("con", "x"), "_con");
        assert_eq!(sanitize_name("LPT1.txt", "x"), "_LPT1.txt");
        assert_eq!(sanitize_name("Console", "x"), "Console");
        let long = "é".repeat(100);
        let clean = sanitize_name(&long, "x");
        assert!(clean.len() <= MAX_NAME_BYTES && clean.chars().all(|c| c == 'é'));
    }

    #[test]
    fn suffixes_keep_dots_already_in_the_name() {
        assert_eq!(
            with_suffix(Path::new("A/01 Mr. Brightside"), "flac"),
            Path::new("A/01 Mr. Brightside.flac")
        );
    }

    #[test]
    fn containers_come_from_the_wire_type_and_are_always_indexable() {
        let flac = Some(MediaContainer::Flac);
        let opus = Some(MediaContainer::Opus);
        assert_eq!(
            download_container(Some("audio/mpeg"), flac),
            Ok(MediaContainer::Mp3),
            "a transcoding server's wire type wins"
        );
        assert_eq!(
            download_container(Some("audio/ogg"), opus),
            Ok(MediaContainer::Opus)
        );
        assert_eq!(download_container(None, flac), Ok(MediaContainer::Flac));
        assert_eq!(
            download_container(Some("application/octet-stream"), flac),
            Ok(MediaContainer::Flac)
        );
        assert_eq!(
            download_container(Some("application/json; charset=utf-8"), flac),
            Err(FetchError::NotAudio)
        );
        assert_eq!(
            download_container(Some("audio/x-unknown"), None),
            Err(FetchError::UnknownContainer)
        );
        for container in [
            MediaContainer::Mp3,
            MediaContainer::Flac,
            MediaContainer::Ogg,
            MediaContainer::Oga,
            MediaContainer::Opus,
            MediaContainer::Wav,
            MediaContainer::Aac,
            MediaContainer::M4a,
            MediaContainer::Aiff,
            MediaContainer::Wma,
        ] {
            assert!(AUDIO_EXTENSIONS.contains(&file_extension(container)));
        }
    }

    fn subsonic_download_request(service: &MockHttpService) -> ResolvedHttpRequest {
        SubsonicClient::new(&service.base_url(), "listener", "secret")
            .expect("client")
            .resolved_download_request("song-1", MediaRepresentation::buffered_from_suffix("flac"))
            .expect("download request")
    }

    fn download_route() -> MockRoute {
        MockRoute::get("/rest/download.view")
            .with_query("id", "song-1")
            .with_query("u", "listener")
    }

    fn files_under(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut folders = vec![root.to_path_buf()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(folder).expect("read folder") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    folders.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files
    }

    #[tokio::test]
    async fn subsonic_download_writes_the_original_file_by_its_content_type() {
        let service = MockHttpService::start(vec![download_route().reply(audio_response())]).await;
        let root = tempfile::tempdir().expect("root");
        let stem = root.path().join("Artist/Album/01 Title");
        let http = UpstreamMediaClient::new().expect("client");

        let target = fetch_to_file(
            &http,
            subsonic_download_request(&service),
            &stem,
            &CancellationToken::new(),
        )
        .await
        .expect("download");
        assert_eq!(target, root.path().join("Artist/Album/01 Title.flac"));
        assert_eq!(std::fs::read_to_string(&target).expect("file"), AUDIO);
        assert_eq!(files_under(root.path()), vec![target]);
        assert!(existing_download(&stem).await);
        service.finish().await;
    }

    #[tokio::test]
    async fn server_refusals_are_told_apart_from_other_failures() {
        // Subsonic reports errors in a JSON body with status 200.
        let subsonic_error = |code: i32| {
            MockResponse::json(serde_json::json!({
                "subsonic-response": {
                    "status": "failed",
                    "error": {"code": code, "message": "Error"}
                }
            }))
        };
        let (responses, expected): (Vec<_>, Vec<_>) = [
            (subsonic_error(50), DownloadOutcome::Refused),
            (subsonic_error(40), DownloadOutcome::Refused),
            (
                MockResponse::status(StatusCode::FORBIDDEN),
                DownloadOutcome::Refused,
            ),
            (
                MockResponse::status(StatusCode::UNAUTHORIZED),
                DownloadOutcome::Refused,
            ),
            (subsonic_error(70), DownloadOutcome::Failed),
            (
                MockResponse::text("<html>Bad gateway</html>")
                    .with_header(header::CONTENT_TYPE, HeaderValue::from_static("text/html")),
                DownloadOutcome::Failed,
            ),
            (
                MockResponse::status(StatusCode::INTERNAL_SERVER_ERROR),
                DownloadOutcome::Failed,
            ),
            (audio_response(), DownloadOutcome::Downloaded),
        ]
        .into_iter()
        .unzip();
        let service = MockHttpService::start(vec![download_route().replies(responses)]).await;
        let root = tempfile::tempdir().expect("root");
        let stem = root.path().join("Artist/Album/01 Title");
        let http = UpstreamMediaClient::new().expect("client");

        let mut outcomes = Vec::new();
        for _ in &expected {
            let request = subsonic_download_request(&service);
            let fetched = fetch_to_file(&http, request, &stem, &CancellationToken::new()).await;
            outcomes.push(fetch_outcome(fetched));
        }
        assert_eq!(outcomes, expected);
        assert_eq!(
            files_under(root.path()),
            vec![root.path().join("Artist/Album/01 Title.flac")],
            "only the audio body is written"
        );
        service.finish().await;
    }

    #[tokio::test]
    async fn cancelling_mid_body_leaves_no_partial_file() {
        let service = MockHttpService::start(vec![
            download_route().reply(audio_response().with_delay(Duration::from_secs(3)))
        ])
        .await;
        let root = tempfile::tempdir().expect("root");
        let stem = root.path().join("Artist/Album/01 Title");
        let part = root.path().join("Artist/Album/01 Title.flac.part");
        let http = UpstreamMediaClient::new().expect("client");
        let cancel = CancellationToken::new();
        let download = tokio::spawn({
            let request = subsonic_download_request(&service);
            let (http, stem, cancel) = (http.clone(), stem.clone(), cancel.clone());
            async move { fetch_to_file(&http, request, &stem, &cancel).await }
        });

        // Headers have arrived and the body is pending once `.part` exists.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !part.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the part file is created after the response headers");
        cancel.cancel();

        assert_eq!(
            download.await.expect("download task"),
            Err(FetchError::Cancelled)
        );
        assert!(files_under(root.path()).is_empty());
        service.finish().await;
    }

    #[tokio::test]
    async fn the_queue_reports_every_item_and_ends_when_it_closes() {
        let root = tempfile::tempdir().expect("root");
        let existing = root.path().join("Artist/Album/01 Title");
        std::fs::create_dir_all(existing.parent().expect("album folder")).expect("album folder");
        // A copy in another container still counts; the registry has no
        // sessions, so the other track fails to resolve.
        std::fs::write(with_suffix(&existing, "mp3"), AUDIO).expect("existing copy");
        let (existing_source, other_source) = (SourceId::random(), SourceId::random());
        let item = |source_id: SourceId, stem: PathBuf| DownloadItem {
            source_id,
            session_epoch: 1,
            track_id: TrackId::remote("song").expect("track"),
            stem,
        };

        for (cancelled, expected) in [
            (false, [DownloadOutcome::Skipped, DownloadOutcome::Failed]),
            (
                true,
                [DownloadOutcome::Cancelled, DownloadOutcome::Cancelled],
            ),
        ] {
            let (queue, queue_rx) = async_channel::unbounded();
            let (outcome_tx, outcomes) = async_channel::unbounded();
            queue
                .try_send(item(existing_source, existing.clone()))
                .expect("queue");
            queue
                .try_send(item(
                    other_source,
                    root.path().join("Artist/Album/02 Other"),
                ))
                .expect("queue");
            drop(queue);
            let cancel = CancellationToken::new();
            if cancelled {
                cancel.cancel();
            }
            run_queue(
                SourceRegistry::new(tokio::runtime::Handle::current()),
                UpstreamMediaClient::new().expect("client"),
                queue_rx,
                cancel,
                outcome_tx,
            )
            .await;
            let mut reported = Vec::new();
            while let Ok(outcome) = outcomes.try_recv() {
                reported.push(outcome);
            }
            // Each outcome names its item's source.
            reported.sort_by_key(|(source_id, _)| *source_id != existing_source);
            assert_eq!(
                reported,
                [(existing_source, expected[0]), (other_source, expected[1])]
            );
        }
    }
}
