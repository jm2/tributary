//! Staging primitives: strict relative-path handling, unique staged leaf
//! names, preserved-sibling disambiguation, and the platform-specific
//! exclusive staged-file creation plus atomic publish/rollback.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Decompose a relative write target into strict normal components.
pub(super) fn strict_relative_components(relative: &Path) -> io::Result<Vec<OsString>> {
    if relative.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write target path must be relative",
        ));
    }
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => components.push(value.to_os_string()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write target path contains a non-normal component",
                ))
            }
        }
    }
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write target path requires a path below the mount root",
        ));
    }
    Ok(components)
}

/// Reassemble components into a relative path.
pub(super) fn assemble_relative(components: &[OsString]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in components {
        path.push(component);
    }
    path
}

/// The parent prefix of a component list, as a relative path.
pub(super) fn parent_components_of(components: &[OsString]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in &components[..components.len().saturating_sub(1)] {
        path.push(component);
    }
    path
}

/// A unique hidden leaf name for a staged temporary file.
pub(super) fn staging_leaf_name() -> OsString {
    let token = Uuid::new_v4();
    let mut name = OsString::from(".tributary-stage-");
    name.push(token.to_string());
    name.push(".tmp");
    name
}

/// Choose a non-colliding `stem (n).ext` sibling for the Preserve policy.
/// Returns the final relative path (parent + candidate leaf) and the parent
/// directory used to stage the temp file, so the rename stays atomic on one
/// filesystem.
pub(super) fn preserved_sibling_path(
    root: &Path,
    parent_components: &Path,
    leaf: &OsString,
) -> io::Result<(PathBuf, PathBuf)> {
    let parent_absolute = if parent_components.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(parent_components)
    };
    let entries = match std::fs::read_dir(&parent_absolute) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "preserve policy requires an existing parent directory",
            ));
        }
        Err(error) => return Err(error),
    };
    let existing: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let original = leaf.to_string_lossy().into_owned();
    let (stem, extension) = match original.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            (stem.to_string(), Some(extension.to_string()))
        }
        _ => (original.clone(), None),
    };
    for index in 1..=u32::MAX {
        let candidate = match &extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        if !existing.iter().any(|name| name == &candidate) {
            // Compose the final relative path (parent + leaf candidate).
            let mut relative = PathBuf::new();
            if !parent_components.as_os_str().is_empty() {
                relative.push(parent_components);
            }
            relative.push(&candidate);
            return Ok((relative, parent_components.to_path_buf()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no preserved name available for conflict",
    ))
}

/// Create each missing directory of a component chain, refusing any
/// intermediate collision with a non-directory.
pub(super) fn create_directory_atomic(root: &Path, components: &[OsString]) -> io::Result<()> {
    let mut path = root.to_path_buf();
    for component in components {
        path.push(component);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "intermediate path is not a directory",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Create the staged file exclusively with `O_CREAT | O_EXCL | O_NOFOLLOW`
/// and mode 0600, opening the parent through a plain descriptor.
#[cfg(unix)]
pub(super) fn create_exclusive_staged_file(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};

    let leaf = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "staged file path is missing a leaf",
        )
    })?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "staged file path is missing a parent",
        )
    })?;
    let parent_file = File::open(parent)?;
    let descriptor = rustix::fs::openat(
        &parent_file,
        leaf,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NOCTTY,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(descriptor))
}

/// Create the staged file exclusively with reparse-point opening rejected.
/// Share everything, including delete: the staged file is a private
/// temporary that must never pin against a concurrent observer, and
/// commit/rollback close the handle before renaming or removing it.
#[cfg(windows)]
pub(super) fn create_exclusive_staged_file(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// The write authority is unsupported on platforms without a staged-file
/// implementation.
#[cfg(not(any(unix, windows)))]
pub(super) fn create_exclusive_staged_file(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "write authority is unsupported on this platform",
    ))
}

/// Drop a staged file, tolerating a concurrent removal.
pub(super) fn rollback_staged(staged_path: &Path) -> io::Result<()> {
    match std::fs::remove_file(staged_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Publish a staged file atomically. POSIX rename is atomic on the same
/// filesystem. Windows std::fs::rename uses MoveFileExW with
/// MOVEFILE_REPLACE_EXISTING semantics, which is likewise atomic on the same
/// volume.
pub(super) fn publish_atomic(staged_path: &Path, final_path: &Path) -> io::Result<()> {
    std::fs::rename(staged_path, final_path)
}
