//! Deterministic large-library fixtures and responsiveness measurement support.
//!
//! This is the fixture lane for the Q4 measured-responsiveness work
//! (<https://github.com/jm2/tributary/issues/275>). It is test-only: it exists so
//! the opt-in measurement tests can build a fixed 10k/100k-track library, slow
//! the traversal/parse path or a `MediaBackend` by a deterministic amount, and
//! record comparable numbers without depending on a real slow disk or network.
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
pub const TRACK_COUNT_ENV: &str = "TRIBUTARY_Q4_TRACKS";

/// Tracks per synthetic album. Kept small so album/artist aggregation has a
/// realistic fan-out rather than one row per album.
const TRACKS_PER_ALBUM: usize = 12;

/// Albums per synthetic artist.
const ALBUMS_PER_ARTIST: usize = 4;

/// Resolve a scratch directory that never lands on the small `/tmp` tmpfs.
///
/// The global environment rule is `${TMPDIR:-/var/tmp}`: `/tmp` is a
/// quota-limited RAM disk that fails writes mid-run once full.
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
        std::env::temp_dir()
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

/// A minimal but valid 8 kHz mono WAV payload.
///
/// `lofty` parses this successfully, so the fixture exercises the production
/// tag-parse path instead of logging unparseable-file skips.
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

/// A fixed, deterministic synthetic library on disk.
///
/// The layout is `Artist{artist:04}/Album{album:04}/Track{track:06}.wav` with
/// [`TRACKS_PER_ALBUM`] tracks per album and [`ALBUMS_PER_ARTIST`] albums per
/// artist, so two runs at the same size produce the same catalogue shape.
pub struct SyntheticLibrary {
    root: PathBuf,
    track_count: usize,
}

impl SyntheticLibrary {
    /// Generate `track_count` minimal WAV files under a fresh scratch root.
    pub fn generate(track_count: usize) -> std::io::Result<Self> {
        let root = scratch_root().join(format!(
            "tributary-q4-library-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root)?;
        let payload = minimal_wav_bytes();
        for index in 0..track_count {
            let album = index / TRACKS_PER_ALBUM;
            let artist = album / ALBUMS_PER_ARTIST;
            let directory = root
                .join(format!("Artist{artist:04}"))
                .join(format!("Album{album:04}"));
            std::fs::create_dir_all(&directory)?;
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
/// Counts the fixed-size `Track` plus the heap bytes of every owned string, so
/// a rebuild can be compared across catalogue sizes without a profiler.
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
        + track.format.as_ref().map_or(0, String::len);
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

        std::fs::remove_dir_all(&directory).expect("remove wav fixture dir");
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
    }
}
