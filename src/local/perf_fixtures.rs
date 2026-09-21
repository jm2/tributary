//! Deterministic large-library fixtures and responsiveness measurement support.
//!
//! This is the fixture lane for the Q4 measured-responsiveness work
//! (<https://github.com/jm2/tributary/issues/275>). It is test-only: it exists so
//! the opt-in measurement tests can build a fixed 10k/100k-track library, slow
//! the traversal/parse path or a `MediaBackend` by a deterministic amount, and
//! record comparable numbers without depending on a real slow disk or network.
//!
//! The synthetic catalogue carries real per-file ID3v2.3 metadata matching the
//! `Artist…/Album…/Track…` directory layout, because the production scan
//! persists what the tag parser reads and the backend aggregates on those
//! persisted values — not on directory names. Fixture fan-out must become
//! catalogue fan-out (12 tracks/album, 4 albums/artist; see
//! [`expected_album_count`] / [`expected_artist_count`]), otherwise album and
//! artist measurements only ever see a single collapsed row.
//!
//! Nothing here ships in a release build. The library is generated on demand
//! under `${TMPDIR:-/var/tmp}` and removed when the fixture is dropped — no
//! synthetic audio is checked into the tree.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::architecture::backend::{BackendResult, MediaBackend};
use crate::architecture::models::{
    Album, Artist, LibraryStats, Rating, RatingCapability, SearchResults, SortField, SortOrder,
    Track,
};
use crate::architecture::TrackId;

/// Default number of synthetic tracks when no size is requested.
pub const DEFAULT_TRACK_COUNT: usize = 10_000;

/// Environment variable selecting the synthetic library size.
///
/// Deliberately distinct from the engine startup benchmark's
/// `TRIBUTARY_Q4_TRACKS` (default 400): both harnesses live in one test
/// binary, and a shared variable with different defaults (10 000 here) would
/// make one measurement run silently resize the other's fixture.
pub const TRACK_COUNT_ENV: &str = "TRIBUTARY_Q4_LIBRARY_TRACKS";

/// Tracks per synthetic album. Kept small so album/artist aggregation has a
/// realistic fan-out rather than one row per album.
const TRACKS_PER_ALBUM: usize = 12;

/// Albums per synthetic artist.
const ALBUMS_PER_ARTIST: usize = 4;

/// Resolve a scratch directory that never lands on the small `/tmp` tmpfs.
///
/// The global environment rule is `${TMPDIR:-/var/tmp}`: `/tmp` is a
/// quota-limited RAM disk that fails writes mid-run once full. Non-unix
/// hosts use `TEMP`/`TMP` (both set by Windows); the final `.` fallback
/// only fires if the platform provides none of these variables, and the
/// fixture cleans up its own scratch trees.
fn scratch_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("TMPDIR") {
        return PathBuf::from(dir);
    }
    #[cfg(unix)]
    {
        PathBuf::from("/var/tmp")
    }
    #[cfg(not(unix))]
    {
        if let Some(dir) = std::env::var_os("TEMP").or_else(|| std::env::var_os("TMP")) {
            return PathBuf::from(dir);
        }
        PathBuf::from(".")
    }
}

/// The requested track count from the environment, or [`DEFAULT_TRACK_COUNT`].
pub fn track_count_from_env() -> usize {
    std::env::var(TRACK_COUNT_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(DEFAULT_TRACK_COUNT)
}

/// A minimal but valid untagged 8 kHz mono WAV payload.
///
/// `lofty` parses this successfully, so the fixture exercises the production
/// tag-parse path instead of logging unparseable-file skips. With no tag
/// chunk the production parser falls back to `Unknown Artist` /
/// `Unknown Album`, which is exactly why [`SyntheticLibrary`] never writes
/// this payload directly: an untagged catalogue collapses every row into one
/// artist and one album (see [`tagged_wav_bytes`]).
pub fn minimal_wav_bytes() -> Vec<u8> {
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
    bytes.push(128);
    bytes
}

/// One ID3v2.3 text frame: 4-byte frame id, big-endian payload size, two
/// flag bytes, then an ISO-8859-1 encoding byte, the text, and the NUL
/// terminator. This is the payload shape the production `lofty` version
/// reads from a WAV `id3 ` chunk.
fn id3v2_text_frame(id: &[u8; 4], value: &str) -> Vec<u8> {
    let payload_len = 1 + value.len() + 1; // encoding byte + text + NUL
    let mut frame = Vec::with_capacity(10 + payload_len);
    frame.extend_from_slice(id);
    frame.extend_from_slice(&(payload_len as u32).to_be_bytes());
    frame.extend_from_slice(&[0x00, 0x00]);
    frame.push(0x00); // ISO-8859-1
    frame.extend_from_slice(value.as_bytes());
    frame.push(0x00);
    frame
}

/// Encode a plain u32 as an ID3v2 syncsafe integer (four 7-bit groups).
fn id3v2_syncsafe(size: u32) -> [u8; 4] {
    [
        ((size >> 21) & 0x7f) as u8,
        ((size >> 14) & 0x7f) as u8,
        ((size >> 7) & 0x7f) as u8,
        (size & 0x7f) as u8,
    ]
}

/// An ID3v2.3 tag wrapping the given prebuilt frames.
fn id3v2_tag(frames: &[Vec<u8>]) -> Vec<u8> {
    let body: usize = frames.iter().map(Vec::len).sum();
    let mut tag = Vec::with_capacity(10 + body);
    tag.extend_from_slice(b"ID3");
    tag.push(0x03); // major version 2.3
    tag.push(0x00); // revision
    tag.push(0x00); // no flags
    tag.extend_from_slice(&id3v2_syncsafe(body as u32));
    for frame in frames {
        tag.extend_from_slice(frame);
    }
    tag
}

/// A valid 8 kHz mono WAV carrying the given metadata as an ID3v2.3 tag in
/// a RIFF `id3 ` chunk.
///
/// The production parser (`tag_parser::parse_audio_file*`) reads title,
/// artist, album, and track number from exactly these frames, so files
/// written with this payload persist as *distinct* catalogue rows instead
/// of collapsing into the `Unknown Artist` / `Unknown Album` bucket. The
/// fixture must use the real tag path: backend album/artist aggregation
/// groups on persisted metadata values, not on directory names.
pub fn tagged_wav_bytes(
    artist: &str,
    album: &str,
    title: &str,
    track_number: usize,
    tracks_on_album: usize,
) -> Vec<u8> {
    let frames = [
        id3v2_text_frame(b"TIT2", title),
        id3v2_text_frame(b"TPE1", artist),
        id3v2_text_frame(b"TALB", album),
        id3v2_text_frame(b"TRCK", &format!("{track_number}/{tracks_on_album}")),
    ];
    let tag = id3v2_tag(&frames);

    // Start from the minimal WAV and append the tag as a trailing `id3 `
    // chunk, widening the RIFF size accordingly. Chunk payloads are
    // word-aligned, so pad the tag to an even length.
    let mut bytes = minimal_wav_bytes();
    let pad = tag.len() % 2;
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().expect("RIFF size field"))
        + 8
        + tag.len() as u32
        + pad as u32;
    bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"id3 ");
    bytes.extend_from_slice(&(tag.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&tag);
    if pad == 1 {
        bytes.push(0x00);
    }
    bytes
}

/// Artist name persisted for the `artist`-th synthetic artist group.
pub fn artist_name_for(artist: usize) -> String {
    format!("Artist{artist:04}")
}

/// Album title persisted for the `album`-th synthetic album group.
pub fn album_title_for(album: usize) -> String {
    format!("Album{album:04}")
}

/// Title persisted for the `index`-th synthetic track.
pub fn track_title_for(index: usize) -> String {
    format!("Track{index:06}")
}

/// Distinct albums the production backend must report for a library of
/// `track_count` fixture files: one album group per [`TRACKS_PER_ALBUM`]
/// tracks, with a final partial group when the count does not divide evenly.
pub fn expected_album_count(track_count: usize) -> usize {
    track_count.div_ceil(TRACKS_PER_ALBUM)
}

/// Distinct artists the production backend must report for a library of
/// `track_count` fixture files: one artist group per [`ALBUMS_PER_ARTIST`]
/// albums, with a final partial group.
pub fn expected_artist_count(track_count: usize) -> usize {
    expected_album_count(track_count).div_ceil(ALBUMS_PER_ARTIST)
}

/// A fixed, deterministic synthetic library on disk.
///
/// The layout is `Artist{artist:04}/Album{album:04}/Track{track:06}.wav` with
/// [`TRACKS_PER_ALBUM`] tracks per album and [`ALBUMS_PER_ARTIST`] albums per
/// artist, so two runs at the same size produce the same catalogue shape.
///
/// Every file also carries ID3v2.3 metadata matching its directory position
/// ([`artist_name_for`] / [`album_title_for`] / [`track_title_for`]), which
/// is what the production parser and backend actually group on. The
/// directory names alone are presentation; without the tags the whole
/// catalogue collapses into one `Unknown Artist` / `Unknown Album` row pair
/// and album/artist aggregation has nothing to fan out over.
pub struct SyntheticLibrary {
    root: PathBuf,
    track_count: usize,
}

impl SyntheticLibrary {
    /// Generate `track_count` tagged minimal WAV files under a fresh scratch
    /// root.
    pub fn generate(track_count: usize) -> std::io::Result<Self> {
        let root = scratch_root().join(format!(
            "tributary-q4-library-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root)?;
        for index in 0..track_count {
            let album = index / TRACKS_PER_ALBUM;
            let artist = album / ALBUMS_PER_ARTIST;
            let directory = root
                .join(artist_name_for(artist))
                .join(album_title_for(album));
            std::fs::create_dir_all(&directory)?;
            // The last album group may be partial; every other group is full.
            let tracks_on_this_album =
                usize::min(TRACKS_PER_ALBUM, track_count - album * TRACKS_PER_ALBUM);
            let payload = tagged_wav_bytes(
                &artist_name_for(artist),
                &album_title_for(album),
                &track_title_for(index),
                index % TRACKS_PER_ALBUM + 1,
                tracks_on_this_album,
            );
            std::fs::write(directory.join(format!("Track{index:06}.wav")), &payload)?;
        }
        Ok(Self { root, track_count })
    }

    /// The root directory to hand to the library scanner.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Number of files generated.
    pub fn track_count(&self) -> usize {
        self.track_count
    }
}

impl Drop for SyntheticLibrary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A `MediaBackend` wrapper that injects a deterministic per-call latency.
///
/// Used to model a slow remote catalogue backend without a live server. It
/// delegates every operation to `inner` after sleeping `delay`, and counts the
/// calls it forwarded so a test can assert the fixture was actually exercised.
pub struct DelayedBackend<B> {
    inner: B,
    delay: Duration,
    calls: AtomicUsize,
}

impl<B: MediaBackend> DelayedBackend<B> {
    /// Wrap `inner`, adding `delay` to every backend call.
    pub fn new(inner: B, delay: Duration) -> Self {
        Self {
            inner,
            delay,
            calls: AtomicUsize::new(0),
        }
    }

    /// Number of calls forwarded through the delay.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    async fn tick(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
    }
}

#[async_trait]
impl<B: MediaBackend + 'static> MediaBackend for DelayedBackend<B> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn backend_type(&self) -> &str {
        self.inner.backend_type()
    }

    async fn ping(&self) -> BackendResult<()> {
        self.tick().await;
        self.inner.ping().await
    }

    async fn search(&self, query: &str, limit: usize) -> BackendResult<SearchResults> {
        self.tick().await;
        self.inner.search(query, limit).await
    }

    async fn list_tracks(&self) -> BackendResult<Vec<Track>> {
        self.tick().await;
        self.inner.list_tracks().await
    }

    fn rating_capability(&self) -> RatingCapability {
        self.inner.rating_capability()
    }

    async fn set_track_rating(
        &self,
        track_id: &TrackId,
        rating: Option<Rating>,
    ) -> BackendResult<Option<Track>> {
        self.tick().await;
        self.inner.set_track_rating(track_id, rating).await
    }

    async fn list_albums(&self, sort: SortField, order: SortOrder) -> BackendResult<Vec<Album>> {
        self.tick().await;
        self.inner.list_albums(sort, order).await
    }

    async fn list_artists(&self) -> BackendResult<Vec<Artist>> {
        self.tick().await;
        self.inner.list_artists().await
    }

    async fn get_album_tracks(&self, album_id: &Uuid) -> BackendResult<Vec<Track>> {
        self.tick().await;
        self.inner.get_album_tracks(album_id).await
    }

    async fn get_artist_tracks(&self, artist_id: &Uuid) -> BackendResult<Vec<Track>> {
        self.tick().await;
        self.inner.get_artist_tracks(artist_id).await
    }

    async fn get_stats(&self) -> BackendResult<LibraryStats> {
        self.tick().await;
        self.inner.get_stats().await
    }
}

/// One recorded responsiveness measurement.
#[derive(Debug, Clone)]
pub struct Metric {
    /// Stable metric identifier, e.g. `scan_elapsed`.
    pub name: &'static str,
    /// Catalogue size the measurement was taken at.
    pub tracks: usize,
    /// Measured value.
    pub value: f64,
    /// Unit for `value`, e.g. `ms`, `bytes`, `tracks_per_second`.
    pub unit: &'static str,
}

impl Metric {
    /// One machine-readable line, stable for budget tooling.
    pub fn line(&self) -> String {
        format!(
            "Q4_METRIC name={} tracks={} value={:.3} unit={}",
            self.name, self.tracks, self.value, self.unit
        )
    }
}

/// A run of measurements plus the environment that produced them.
#[derive(Debug, Default)]
pub struct ResponsivenessReport {
    environment: String,
    metrics: Vec<Metric>,
}

impl ResponsivenessReport {
    /// Start a report identified by a runner label.
    pub fn new(environment: impl Into<String>) -> Self {
        Self {
            environment: environment.into(),
            metrics: Vec::new(),
        }
    }

    /// Record one measurement.
    pub fn record(&mut self, name: &'static str, tracks: usize, value: f64, unit: &'static str) {
        self.metrics.push(Metric {
            name,
            tracks,
            value,
            unit,
        });
    }

    /// Record an elapsed duration in milliseconds.
    pub fn record_ms(&mut self, name: &'static str, tracks: usize, elapsed: Duration) {
        self.record(name, tracks, elapsed.as_secs_f64() * 1_000.0, "ms");
    }

    /// Render the report as stable, line-oriented output.
    pub fn render(&self) -> String {
        let mut rendered = format!("Q4_ENVIRONMENT runner={}\n", self.environment);
        for metric in &self.metrics {
            rendered.push_str(&metric.line());
            rendered.push('\n');
        }
        rendered
    }

    /// Recorded metrics.
    pub fn metrics(&self) -> &[Metric] {
        &self.metrics
    }
}

/// Approximate bytes retained by a published catalogue snapshot.
///
/// Counts the fixed-size `Track` plus the heap bytes of every owned string:
/// the eight text fields below, the backend-native track id (a heap `String`
/// behind [`TrackId`], stored verbatim for every local row), and the
/// credential-free `stream_url` / `cover_art_url` references when present —
/// so a rebuild can be compared across catalogue sizes without a profiler.
pub fn catalogue_bytes(tracks: &[Track]) -> usize {
    tracks.iter().map(track_bytes).sum()
}

fn track_bytes(track: &Track) -> usize {
    let owned = track.title.len()
        + track.artist_name.len()
        + track.album_title.len()
        + track.album_artist_name.as_ref().map_or(0, String::len)
        + track.composer.as_ref().map_or(0, String::len)
        + track.genre.as_ref().map_or(0, String::len)
        + track.file_path.as_ref().map_or(0, String::len)
        + track.format.as_ref().map_or(0, String::len)
        + track
            .native_track_id
            .as_ref()
            .map_or(0, |id| id.as_str().len())
        + track
            .stream_url
            .as_ref()
            .map_or(0, |url| url.as_str().len())
        + track
            .cover_art_url
            .as_ref()
            .map_or(0, |url| url.as_str().len());
    std::mem::size_of::<Track>() + owned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_wav_parses_through_the_production_parser() {
        let directory = scratch_root().join(format!(
            "tributary-q4-wav-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create wav fixture dir");
        let path = directory.join("track.wav");
        std::fs::write(&path, minimal_wav_bytes()).expect("write wav fixture");

        let parsed =
            super::super::tag_parser::parse_audio_file(&path).expect("parse minimal WAV fixture");
        assert_eq!(parsed.format, "WAV");
        assert_eq!(parsed.file_size_bytes, Some(45));
        // The untagged payload is why it must never be used for a catalogue:
        // production falls back to the Unknown Artist / Unknown Album bucket,
        // which would collapse the whole library into one artist and album.
        assert!(!parsed.artist_from_tag);
        assert!(!parsed.album_from_tag);
        assert_eq!(parsed.artist_name, "Unknown Artist");
        assert_eq!(parsed.album_title, "Unknown Album");

        std::fs::remove_dir_all(&directory).expect("remove wav fixture dir");
    }

    #[test]
    fn tagged_wav_metadata_is_read_by_the_production_parser() {
        let directory = scratch_root().join(format!(
            "tributary-q4-tagged-wav-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create tagged wav fixture dir");
        let path = directory.join("track.wav");
        std::fs::write(
            &path,
            tagged_wav_bytes("Artist0007", "Album0003", "Track000043", 7, 12),
        )
        .expect("write tagged wav fixture");

        let parsed =
            super::super::tag_parser::parse_audio_file(&path).expect("parse tagged WAV fixture");
        assert_eq!(parsed.format, "WAV");
        assert!(parsed.title_from_tag);
        assert!(parsed.artist_from_tag);
        assert!(parsed.album_from_tag);
        assert_eq!(parsed.title, "Track000043");
        assert_eq!(parsed.artist_name, "Artist0007");
        assert_eq!(parsed.album_title, "Album0003");
        assert_eq!(parsed.track_number, Some(7));

        std::fs::remove_dir_all(&directory).expect("remove tagged wav fixture dir");
    }

    #[test]
    fn synthetic_library_has_the_requested_shape() {
        let library = SyntheticLibrary::generate(24).expect("generate fixture");
        assert_eq!(library.track_count(), 24);
        let album0 = library.root().join("Artist0000").join("Album0000");
        assert!(album0.join("Track000000.wav").exists());
        // 24 tracks / 12 per album = 2 albums, both under the first artist.
        let album1 = library.root().join("Artist0000").join("Album0001");
        assert!(album1.join("Track000012.wav").exists());

        // Directory position and persisted metadata must agree: the scan
        // groups on the parsed tag values, so a directory fan-out that does
        // not reach the tags produces no catalogue fan-out.
        let parsed = super::super::tag_parser::parse_audio_file(&album1.join("Track000012.wav"))
            .expect("parse generated fixture file");
        assert_eq!(parsed.title, "Track000012");
        assert_eq!(parsed.artist_name, "Artist0000");
        assert_eq!(parsed.album_title, "Album0001");
        assert_eq!(parsed.track_number, Some(1));
    }

    #[test]
    fn expected_catalogue_cardinalities_match_the_documented_layout() {
        // 12 tracks per album, 4 albums per artist, final partial groups.
        // These are the numbers the refinery acceptance requires the
        // measurement to assert through the real backend.
        assert_eq!(expected_album_count(10_000), 834);
        assert_eq!(expected_artist_count(10_000), 209);
        assert_eq!(expected_album_count(100_000), 8_334);
        assert_eq!(expected_artist_count(100_000), 2_084);

        // Final partial groups: 10_000 = 833 full albums + 4 tracks; the
        // 834 albums = 208 full artists (4 albums each) + 1 artist with the
        // final 2 albums. Same shape at 100k.
        assert_eq!(10_000 - 833 * TRACKS_PER_ALBUM, 4);
        assert_eq!(834 - 208 * ALBUMS_PER_ARTIST, 2);
        assert_eq!(100_000 - 8_333 * TRACKS_PER_ALBUM, 4);
        assert_eq!(8_334 - 2_083 * ALBUMS_PER_ARTIST, 2);

        // Small sizes stay exact too.
        assert_eq!(expected_album_count(24), 2);
        assert_eq!(expected_artist_count(24), 1);
        assert_eq!(expected_album_count(100), 9);
        assert_eq!(expected_artist_count(100), 3);
    }

    #[test]
    fn report_renders_stable_metric_lines() {
        let mut report = ResponsivenessReport::new("unit-test-runner");
        report.record_ms("scan_elapsed", 10_000, Duration::from_millis(250));
        let rendered = report.render();
        assert!(rendered.contains("Q4_ENVIRONMENT runner=unit-test-runner"));
        assert!(rendered.contains("Q4_METRIC name=scan_elapsed tracks=10000 value=250.000 unit=ms"));
    }

    #[test]
    fn catalogue_bytes_counts_owned_strings() {
        let mut track = Track {
            id: Uuid::nil(),
            native_track_id: None,
            title: "t".repeat(100),
            artist_name: String::new(),
            album_artist_name: None,
            artist_id: None,
            album_title: String::new(),
            album_id: None,
            track_number: None,
            disc_number: None,
            duration_secs: None,
            composer: None,
            genre: None,
            year: None,
            file_path: None,
            stream_url: None,
            cover_art_url: None,
            date_added: None,
            date_modified: None,
            bitrate_kbps: None,
            sample_rate_hz: None,
            format: None,
            play_count: None,
            rating: crate::architecture::models::TrackRating::unsupported(),
            last_played: None,
        };
        let empty = catalogue_bytes(std::slice::from_ref(&track));
        track.title = "t".repeat(50);
        let half = catalogue_bytes(std::slice::from_ref(&track));
        assert_eq!(empty - half, 50);
        // The backend-native id is a heap `String` per row (the scan stores
        // the SQLite id verbatim), so omitting it underreported the metric
        // by its length × track count (refinery F1, PR #285).
        track.native_track_id = Some(TrackId::new("native-track-id").expect("valid track id"));
        let with_id = catalogue_bytes(std::slice::from_ref(&track));
        assert_eq!(with_id - half, "native-track-id".len());
        // Credential-free URL references are owned strings too; keep the
        // `catalogue_bytes` coverage claim exact.
        track.stream_url =
            Some(url::Url::parse("https://stream.example/a.mp3").expect("valid stream url"));
        let with_stream = catalogue_bytes(std::slice::from_ref(&track));
        assert_eq!(with_stream - with_id, "https://stream.example/a.mp3".len());
    }
}
