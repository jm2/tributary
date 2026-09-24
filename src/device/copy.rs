//! Copy tracks and playlists onto a mounted device's filesystem.
//!
//! Files land at `Music/<Album Artist or Artist>/<Album>/<NN Title>.<ext>`
//! with every component made safe for FAT and exFAT. Each file is written to
//! a `.part` sibling, flushed to the device, then renamed into place, so an
//! interrupted copy never leaves a truncated track under its final name. A
//! destination that already has the source's size is kept as it is.
//!
//! Everything here blocks on filesystem I/O and belongs on a worker thread.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use gtk::gio;
use gtk::prelude::*;

use crate::local::resolver::MountedRootAuthority;

const MUSIC_DIR: &str = "Music";
const PLAYLISTS_DIR: &str = "Playlists";
/// Keeps each name well inside FAT's 255 UTF-16 unit limit and leaves room
/// for the whole path on players with short path buffers.
const MAX_COMPONENT_CHARS: usize = 100;
const CHUNK_BYTES: usize = 1 << 20;
const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// One file to copy and the tags that name its destination.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopySource {
    pub path: PathBuf,
    pub album_artist: String,
    pub artist: String,
    pub album: String,
    pub title: String,
    pub track_number: u32,
}

/// A copy of `sources` onto the mounted filesystem at `device_root`.
#[derive(Clone, Debug)]
pub struct CopyRequest {
    pub device_root: PathBuf,
    pub sources: Vec<CopySource>,
    /// Also write `Music/Playlists/<name>.m3u8` listing the sources in order.
    pub playlist_name: Option<String>,
    /// Folder name for a track without a usable artist.
    pub unknown_artist: String,
    /// Folder name for a track without a usable album.
    pub unknown_album: String,
}

/// Running totals reported while a copy is in progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyProgress {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
}

/// What happened to each distinct destination file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopySummary {
    pub copied: usize,
    /// Already on the device with the source's size.
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
}

/// Why a copy refused to start. Nothing has been written when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyRefusal {
    DeviceUnavailable,
    ReadOnly,
    InsufficientSpace { required: u64, available: u64 },
}

/// Free space and writability of the device's filesystem.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceSpace {
    /// `None` when the filesystem does not report it.
    pub free: Option<u64>,
    pub read_only: bool,
}

/// Ask GIO for the free space and read-only state of the filesystem at `root`.
pub fn query_device_space(root: &Path) -> DeviceSpace {
    let attributes = format!(
        "{},{}",
        gio::FILE_ATTRIBUTE_FILESYSTEM_FREE,
        gio::FILE_ATTRIBUTE_FILESYSTEM_READONLY
    );
    match gio::File::for_path(root).query_filesystem_info(&attributes, gio::Cancellable::NONE) {
        Ok(info) => DeviceSpace {
            free: info
                .has_attribute(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE)
                .then(|| info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE)),
            read_only: info.boolean(gio::FILE_ATTRIBUTE_FILESYSTEM_READONLY),
        },
        Err(error) => {
            tracing::warn!(%error, "Could not query the device's free space");
            DeviceSpace::default()
        }
    }
}

/// Make one path component safe on FAT and exFAT.
///
/// Strips reserved and control characters, trims leading and trailing dots
/// and spaces (which Windows drops or hides), caps the length, and prefixes
/// DOS device names such as `CON`. The result may be empty.
pub fn sanitize_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && !FORBIDDEN_CHARS.contains(c))
        .collect();
    let capped: String = trim_edges(&cleaned)
        .chars()
        .take(MAX_COMPONENT_CHARS)
        .collect();
    let mut name = trim_edges(&capped).to_owned();
    let stem = name.split('.').next().unwrap_or_default();
    if RESERVED_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        name.insert(0, '_');
    }
    name
}

fn trim_edges(name: &str) -> &str {
    name.trim_matches(|c: char| c == '.' || c.is_whitespace())
}

fn first_usable<'a>(candidates: impl IntoIterator<Item = &'a str>) -> Option<String> {
    candidates
        .into_iter()
        .map(sanitize_component)
        .find(|name| !name.is_empty())
}

/// Device-relative location of one track below `Music/`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Destination {
    artist: String,
    album: String,
    file: String,
}

impl Destination {
    /// `copy` numbers a second distinct source that would otherwise land on
    /// the same name, which FAT compares without case.
    fn for_source(source: &CopySource, request: &CopyRequest, copy: usize) -> Self {
        let artist = first_usable([
            source.album_artist.as_str(),
            source.artist.as_str(),
            request.unknown_artist.as_str(),
        ])
        .unwrap_or_else(|| "_".to_owned());
        let album = first_usable([source.album.as_str(), request.unknown_album.as_str()])
            .unwrap_or_else(|| "_".to_owned());
        let file_stem = source
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = first_usable([source.title.as_str(), file_stem.as_str()])
            .unwrap_or_else(|| "Track".to_owned());

        let number = if source.track_number > 0 {
            format!("{:02} ", source.track_number)
        } else {
            String::new()
        };
        let suffix = if copy > 1 {
            format!(" ({copy})")
        } else {
            String::new()
        };
        let extension = source
            .path
            .extension()
            .map(|extension| sanitize_component(&extension.to_string_lossy()))
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!(".{extension}"))
            .unwrap_or_default();
        let file = format!("{number}{title}{suffix}{extension}");
        Self {
            artist,
            album,
            file,
        }
    }

    fn relative_path(&self) -> PathBuf {
        [MUSIC_DIR, &self.artist, &self.album, &self.file]
            .iter()
            .collect()
    }

    /// Entry for an `.m3u8` stored in `Music/Playlists/`.
    fn playlist_entry(&self) -> String {
        format!("../{}/{}/{}", self.artist, self.album, self.file)
    }
}

struct PlannedFile {
    source: PathBuf,
    destination: Destination,
    size: u64,
    present: bool,
}

struct Plan {
    files: Vec<PlannedFile>,
    /// Index into `files` for each requested source, in request order;
    /// `None` for a source that could not be read.
    entries: Vec<Option<usize>>,
}

fn plan(request: &CopyRequest) -> Plan {
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut claimed: HashMap<String, usize> = HashMap::new();
    let mut entries = Vec::with_capacity(request.sources.len());

    for source in &request.sources {
        let size = match fs::metadata(&source.path) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            _ => {
                entries.push(None);
                continue;
            }
        };
        let mut copy = 1;
        let index = loop {
            let destination = Destination::for_source(source, request, copy);
            let key = destination.relative_path().to_string_lossy().to_lowercase();
            match claimed.get(&key) {
                Some(&index) if files[index].source == source.path => break index,
                Some(_) => copy += 1,
                None => {
                    let present =
                        fs::metadata(request.device_root.join(destination.relative_path()))
                            .is_ok_and(|existing| existing.is_file() && existing.len() == size);
                    claimed.insert(key, files.len());
                    files.push(PlannedFile {
                        source: source.path.clone(),
                        destination,
                        size,
                        present,
                    });
                    break files.len() - 1;
                }
            }
        };
        entries.push(Some(index));
    }
    Plan { files, entries }
}

/// Copy `request` onto its device.
///
/// `space` reports the device's free space; the copy is refused before
/// anything is written when the files still missing do not fit. Setting
/// `cancel` stops at the next chunk: files already renamed into place stay,
/// and only the `.part` file in flight is removed. The destination mount is
/// re-verified before every file, and a device that was removed or replaced
/// fails the remaining files instead of writing to whatever is now mounted.
pub fn run(
    request: &CopyRequest,
    space: impl FnOnce(&Path) -> DeviceSpace,
    cancel: &AtomicBool,
    mut progress: impl FnMut(CopyProgress),
) -> Result<CopySummary, CopyRefusal> {
    let authority = MountedRootAuthority::acquire(&request.device_root)
        .map_err(|_| CopyRefusal::DeviceUnavailable)?;
    let plan = plan(request);
    let required: u64 = plan
        .files
        .iter()
        .filter(|file| !file.present)
        .map(|file| file.size)
        .sum();
    let space = space(&request.device_root);
    if space.read_only {
        return Err(CopyRefusal::ReadOnly);
    }
    if let Some(available) = space.free.filter(|&free| free < required) {
        return Err(CopyRefusal::InsufficientSpace {
            required,
            available,
        });
    }

    let mut summary = CopySummary {
        failed: plan.entries.iter().filter(|entry| entry.is_none()).count(),
        ..CopySummary::default()
    };
    let mut on_device = vec![false; plan.files.len()];
    let mut status = CopyProgress {
        files_total: plan.files.len(),
        ..CopyProgress::default()
    };
    let mut buffer = vec![0; CHUNK_BYTES];
    let mut device_lost = false;

    for (index, file) in plan.files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        if file.present {
            summary.skipped += 1;
            on_device[index] = true;
        } else if authority.validate().is_err() {
            tracing::warn!("Copy destination was removed or replaced; stopping");
            summary.failed += plan.files.len() - index;
            device_lost = true;
            break;
        } else {
            let destination = request.device_root.join(file.destination.relative_path());
            let result = copy_file(&file.source, &destination, cancel, &mut buffer, |bytes| {
                status.bytes_done += bytes;
                progress(status);
            });
            match result {
                Ok(()) => {
                    summary.copied += 1;
                    on_device[index] = true;
                }
                Err(FileError::Cancelled) => {
                    summary.cancelled = true;
                    break;
                }
                Err(FileError::Io(error)) => {
                    tracing::warn!(%error, destination = %destination.display(), "Could not copy a track to the device");
                    summary.failed += 1;
                }
            }
        }
        status.files_done = index + 1;
        progress(status);
    }

    if let Some(name) = &request.playlist_name {
        if !summary.cancelled && !device_lost {
            let document = playlist_document(&plan, &on_device);
            let file_name = format!(
                "{}.m3u8",
                first_usable([name.as_str()]).unwrap_or_else(|| "Playlist".to_owned())
            );
            let path = request
                .device_root
                .join(MUSIC_DIR)
                .join(PLAYLISTS_DIR)
                .join(file_name);
            let written = authority.validate().map_err(FileError::Io).and_then(|()| {
                write_via_part(&path, |output| Ok(output.write_all(document.as_bytes())?))
            });
            if let Err(FileError::Io(error)) = written {
                tracing::warn!(%error, "Could not write the playlist to the device");
                summary.failed += 1;
            }
        }
    }
    Ok(summary)
}

/// An extended M3U listing, in playlist order, every entry now on the device.
fn playlist_document(plan: &Plan, on_device: &[bool]) -> String {
    let mut document = String::from("#EXTM3U\n");
    for &index in plan.entries.iter().flatten() {
        if on_device[index] {
            document.push_str(&plan.files[index].destination.playlist_entry());
            document.push('\n');
        }
    }
    document
}

enum FileError {
    Cancelled,
    Io(io::Error),
}

impl From<io::Error> for FileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Write `destination` through a `.part` sibling that `fill` writes, which
/// is synced and then renamed into place. Any failure removes the `.part`.
fn write_via_part(
    destination: &Path,
    fill: impl FnOnce(&mut File) -> Result<(), FileError>,
) -> Result<(), FileError> {
    let mut part_name = destination.file_name().unwrap_or_default().to_os_string();
    part_name.push(".part");
    let part = destination.with_file_name(part_name);
    let result = destination
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| File::create(&part))
        .map_err(FileError::from)
        .and_then(|mut output| {
            fill(&mut output)?;
            output.sync_all()?;
            Ok(())
        })
        .and_then(|()| Ok(fs::rename(&part, destination)?));
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

/// Stream `source` to `destination`, checking `cancel` before every chunk.
fn copy_file(
    source: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    buffer: &mut [u8],
    mut on_bytes: impl FnMut(u64),
) -> Result<(), FileError> {
    write_via_part(destination, |output| {
        let mut input = File::open(source)?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(FileError::Cancelled);
            }
            let read = match input.read(buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            output.write_all(&buffer[..read])?;
            on_bytes(read as u64);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_for(device: &Path, sources: Vec<CopySource>) -> CopyRequest {
        CopyRequest {
            device_root: device.to_path_buf(),
            sources,
            playlist_name: None,
            unknown_artist: "Unknown Artist".to_owned(),
            unknown_album: "Unknown Album".to_owned(),
        }
    }

    fn source(library: &Path, file: &str, bytes: usize, title: &str, number: u32) -> CopySource {
        let path = library.join(file);
        fs::write(&path, vec![7_u8; bytes]).expect("write source");
        CopySource {
            path,
            artist: "Artist".to_owned(),
            album: "Album".to_owned(),
            title: title.to_owned(),
            track_number: number,
            ..CopySource::default()
        }
    }

    fn unlimited(_: &Path) -> DeviceSpace {
        DeviceSpace::default()
    }

    fn run_to_end(request: &CopyRequest) -> CopySummary {
        run(request, unlimited, &AtomicBool::new(false), |_| {}).expect("copy starts")
    }

    fn album_dir(device: &Path) -> PathBuf {
        device.join("Music").join("Artist").join("Album")
    }

    fn part_files(dir: &Path) -> Vec<PathBuf> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "part")
            })
            .collect()
    }

    #[test]
    fn sanitizing_removes_what_fat_and_exfat_reject() {
        assert_eq!(
            sanitize_component(r#"AC/DC: "Live" <1991> | What? *"#),
            "ACDC Live 1991  What"
        );
        assert_eq!(sanitize_component("tab\there\u{7}\n"), "tabhere");
        assert_eq!(
            sanitize_component("  ...Trailing dots... "),
            "Trailing dots"
        );
        assert_eq!(sanitize_component("con"), "_con");
        assert_eq!(sanitize_component("Nul.txt"), "_Nul.txt");
        assert_eq!(sanitize_component("LPT9"), "_LPT9");
        assert_eq!(sanitize_component("Console"), "Console");
        assert_eq!(sanitize_component("?*"), "");

        assert_eq!(sanitize_component(&"é".repeat(150)), "é".repeat(100));
        // A cut that ends on a space is trimmed again.
        let spaced = format!("{} tail", "a".repeat(99));
        assert_eq!(sanitize_component(&spaced), "a".repeat(99));
    }

    #[test]
    fn layout_prefers_album_artist_then_artist_then_the_fallbacks() {
        let request = request_for(Path::new("/device"), Vec::new());
        let mut source = CopySource {
            path: PathBuf::from("/library/x/song.FLAC"),
            album_artist: "Various Artists".to_owned(),
            artist: "Singer".to_owned(),
            album: "Hits: Vol. 1".to_owned(),
            title: "Song?".to_owned(),
            track_number: 3,
        };
        assert_eq!(
            Destination::for_source(&source, &request, 1).relative_path(),
            Path::new("Music/Various Artists/Hits Vol. 1/03 Song.FLAC")
        );

        source.album_artist = " ".to_owned();
        source.track_number = 0;
        assert_eq!(
            Destination::for_source(&source, &request, 1).relative_path(),
            Path::new("Music/Singer/Hits Vol. 1/Song.FLAC")
        );

        source.artist.clear();
        source.album = "...".to_owned();
        source.title.clear();
        let destination = Destination::for_source(&source, &request, 2);
        assert_eq!(
            destination.relative_path(),
            Path::new("Music/Unknown Artist/Unknown Album/song (2).FLAC")
        );
        assert_eq!(
            destination.playlist_entry(),
            "../Unknown Artist/Unknown Album/song (2).FLAC"
        );
    }

    #[test]
    fn existing_files_of_the_same_size_are_skipped_and_others_replaced() {
        let device = tempfile::tempdir().expect("device");
        let library = tempfile::tempdir().expect("library");
        let request = request_for(
            device.path(),
            vec![
                source(library.path(), "a.mp3", 10, "One", 1),
                source(library.path(), "b.mp3", 20, "Two", 2),
            ],
        );
        let present = album_dir(device.path()).join("02 Two.mp3");
        fs::create_dir_all(album_dir(device.path())).expect("album dir");
        fs::write(&present, vec![0_u8; 20]).expect("same-size file");

        assert_eq!(
            run_to_end(&request),
            CopySummary {
                copied: 1,
                skipped: 1,
                ..CopySummary::default()
            }
        );
        assert_eq!(
            fs::read(album_dir(device.path()).join("01 One.mp3")).expect("copied"),
            vec![7_u8; 10]
        );
        assert_eq!(fs::read(&present).expect("kept"), vec![0_u8; 20]);

        fs::write(&present, b"stale").expect("different-size file");
        assert_eq!(
            run_to_end(&request),
            CopySummary {
                copied: 1,
                skipped: 1,
                ..CopySummary::default()
            }
        );
        assert_eq!(fs::read(&present).expect("replaced"), vec![7_u8; 20]);
        assert!(part_files(device.path()).is_empty());
    }

    #[test]
    fn cancelling_keeps_finished_files_and_removes_only_the_partial_one() {
        let device = tempfile::tempdir().expect("device");
        let library = tempfile::tempdir().expect("library");
        let request = request_for(
            device.path(),
            vec![
                source(library.path(), "a.mp3", 10, "One", 1),
                source(library.path(), "b.mp3", 3 * CHUNK_BYTES, "Two", 2),
            ],
        );
        let cancel = AtomicBool::new(false);

        let summary = run(&request, unlimited, &cancel, |progress| {
            // The first chunk of the second file has been written.
            if progress.bytes_done > 10 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .expect("copy starts");

        assert_eq!(
            summary,
            CopySummary {
                copied: 1,
                cancelled: true,
                ..CopySummary::default()
            }
        );
        assert!(album_dir(device.path()).join("01 One.mp3").is_file());
        assert!(!album_dir(device.path()).join("02 Two.mp3").exists());
        assert!(part_files(device.path()).is_empty());
    }

    #[test]
    fn refusals_happen_before_anything_is_written() {
        let device = tempfile::tempdir().expect("device");
        let library = tempfile::tempdir().expect("library");
        let request = request_for(
            device.path(),
            vec![
                source(library.path(), "a.mp3", 10, "One", 1),
                source(library.path(), "b.mp3", 20, "Two", 2),
            ],
        );
        let never = AtomicBool::new(false);

        let short = |_: &Path| DeviceSpace {
            free: Some(29),
            read_only: false,
        };
        assert_eq!(
            run(&request, short, &never, |_| {}),
            Err(CopyRefusal::InsufficientSpace {
                required: 30,
                available: 29,
            })
        );
        let read_only = |_: &Path| DeviceSpace {
            free: Some(1 << 30),
            read_only: true,
        };
        assert_eq!(
            run(&request, read_only, &never, |_| {}),
            Err(CopyRefusal::ReadOnly)
        );
        assert!(!device.path().join("Music").exists());

        let removed = request_for(&device.path().join("unplugged"), request.sources.clone());
        assert_eq!(
            run(&removed, unlimited, &never, |_| {}),
            Err(CopyRefusal::DeviceUnavailable)
        );

        // Files already on the device need no space.
        fs::create_dir_all(album_dir(device.path())).expect("album dir");
        fs::write(album_dir(device.path()).join("02 Two.mp3"), [0_u8; 20]).expect("present");
        let exact = |_: &Path| DeviceSpace {
            free: Some(10),
            read_only: false,
        };
        assert_eq!(
            run(&request, exact, &never, |_| {}).map(|summary| summary.copied),
            Ok(1)
        );
    }

    #[test]
    fn playlist_lists_tracks_on_the_device_in_playlist_order() {
        let device = tempfile::tempdir().expect("device");
        let library = tempfile::tempdir().expect("library");
        let one = source(library.path(), "a.mp3", 10, "One", 1);
        let two = source(library.path(), "b.mp3", 20, "Two", 2);
        let same_tags = source(library.path(), "c.mp3", 5, "One", 1);
        let missing = CopySource {
            path: library.path().join("gone.mp3"),
            ..one.clone()
        };
        fs::create_dir_all(album_dir(device.path())).expect("album dir");
        fs::write(album_dir(device.path()).join("02 Two.mp3"), [0_u8; 20]).expect("present");
        let mut request = request_for(
            device.path(),
            vec![two.clone(), missing, one, same_tags, two],
        );
        request.playlist_name = Some("Road: Trip?".to_owned());

        assert_eq!(
            run_to_end(&request),
            CopySummary {
                copied: 2,
                skipped: 1,
                failed: 1,
                cancelled: false,
            }
        );
        assert_eq!(
            fs::read_to_string(device.path().join("Music/Playlists/Road Trip.m3u8"))
                .expect("playlist"),
            "#EXTM3U\n\
             ../Artist/Album/02 Two.mp3\n\
             ../Artist/Album/01 One.mp3\n\
             ../Artist/Album/01 One (2).mp3\n\
             ../Artist/Album/02 Two.mp3\n"
        );
        assert!(part_files(device.path()).is_empty());
    }
}
