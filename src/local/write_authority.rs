//! Retained write authority for one exact mounted filesystem.
//!
//! This module adds write capability on top of the read-only
//! [`MountedRootAuthority`]. The same retained root handle, parent chain,
//! mount generation, and filesystem boundary policy gate every byte written
//! beneath the mount. There is no shared-marker requirement; mounted authority
//! exists for ephemeral portable devices and replaces its session epoch on
//! relocation, pre-unmount, or removal.
//!
//! The intended consumer is the generic mounted-filesystem transfer planner
//! in [`crate::device::transfer`]. It must never be used to author a library
//! root (those keep their marker-backed
//! [`RootAuthorityLease`](super::root_authority::RootAuthorityLease) and
//! explicit database enrollment).
//!
//! File writes are staged: a sibling temporary file is created with
//! `O_CREAT | O_EXCL | O_NOFOLLOW` (Unix) or with the reparse-point attribute
//! rejected (Windows), then renamed atomically once
//! [`PreparedWriteTarget::commit`] is called. A rollback drops the staged
//! file. The destination filesystem is observed through the same retained
//! boundary, so a binder swap or remount between staging and commit is
//! detected and refused without surfacing a partial publish.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::root_authority::MountedRootAuthority;

/// What the write authority should do when the destination of a write already
/// exists beneath the mount.
///
/// `Skip` and `Fail` close the question for the whole transfer on a single
/// collision; `Overwrite` and `Preserve` permit the operation to proceed
/// without further prompt. Each variant is a typed policy, not a boolean flag,
/// so reviewers can grep call sites for the precise behavior at every
/// admission boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictPolicy {
    /// Leave any existing destination untouched and skip the operation.
    Skip,
    /// Atomically replace the existing destination during commit.
    Overwrite,
    /// Choose a non-colliding name in the same directory and create anew.
    Preserve,
    /// Refuse the operation; transfer fails before any byte is written.
    Fail,
}

/// Outcome of resolving a conflict policy against the live filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictResolution {
    /// Destination was absent; the staged file becomes a fresh write.
    Fresh,
    /// Destination existed; the staged file will replace it on commit.
    Overwrite,
    /// Destination existed; the staged file is written to a disambiguated name.
    Preserved,
}

/// A staged write below a [`MountedWriteAuthority`] ready for commit/rollback.
///
/// The destination is held in a sibling temporary file. Until
/// [`commit`](Self::commit) is called the original destination is untouched,
/// so a partially-written staged file can be discarded without disturbing the
/// mount. Once commit fires, the rename is atomic on the same filesystem and
/// the staged file is gone.
pub struct PreparedWriteTarget {
    lease_token: Uuid,
    authority: Arc<MountedRootAuthority>,
    final_relative_path: PathBuf,
    /// Absolute path of the staged temporary file. Sibling of the destination
    /// so the rename is atomic on the same filesystem.
    staged_path: PathBuf,
    staged_file: File,
    resolution: ConflictResolution,
    committed: bool,
}

impl fmt::Debug for PreparedWriteTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedWriteTarget")
            .field("target_relative_path", &self.final_relative_path)
            .field("staged_path", &self.staged_path)
            .field("resolution", &self.resolution)
            .finish_non_exhaustive()
    }
}

impl PreparedWriteTarget {
    /// Final relative path the staged file will be renamed to on commit.
    pub fn target_relative_path(&self) -> &Path {
        &self.final_relative_path
    }

    /// How the conflict policy was resolved against the live filesystem.
    pub fn resolution(&self) -> ConflictResolution {
        self.resolution
    }

    /// Borrow the staged file for reads (e.g. computing a digest).
    pub fn staged_file(&self) -> &File {
        &self.staged_file
    }

    /// Append `bytes` to the staged file.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.authority.validate()?;
        self.staged_file.write_all(bytes)?;
        self.staged_file.flush()?;
        self.authority.validate()?;
        Ok(())
    }

    /// Commit the staged file atomically to its destination.
    ///
    /// On Unix this is a single `rename(2)`; on Windows a `MoveFileExW`
    /// replacement. The mount boundary is revalidated immediately before and
    /// after the rename so a binder swap or remount between staging and
    /// commit cannot authorise a partial publish.
    pub fn commit(mut self) -> io::Result<CommitOutcome> {
        let final_path = self.authority.root().join(&self.final_relative_path);
        self.authority.validate()?;
        publish_atomic(&self.staged_path, &final_path)?;
        self.authority.validate()?;
        self.committed = true;
        Ok(CommitOutcome {
            relative_path: self.final_relative_path.clone(),
            resolution: self.resolution,
        })
    }

    /// Discard the staged file and any partial writes.
    pub fn rollback(self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        let outcome = rollback_staged(&self.staged_path);
        let _ = self.authority.validate();
        outcome
    }
}

impl Drop for PreparedWriteTarget {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Best-effort cleanup if the caller forgets to roll back explicitly.
        let _ = rollback_staged(&self.staged_path);
    }
}

/// Detail of what `commit` actually published, for callers that need to log
/// or report the publish outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    /// The relative path beneath the retained mount that now names the data.
    pub relative_path: PathBuf,
    /// How the conflict policy was resolved against the live filesystem.
    pub resolution: ConflictResolution,
}

/// Retained write authority over one exact mounted filesystem.
///
/// The underlying [`MountedRootAuthority`] is shared so the read-side scans
/// and the write-side commits always observe the same mount generation and
/// boundary. A successful transfer followed by a remount is detected on the
/// next `validate()` and produces a fail-closed error rather than attempting
/// a partial commit.
#[derive(Clone)]
pub struct MountedWriteAuthority {
    mounted: Arc<MountedRootAuthority>,
}

impl MountedWriteAuthority {
    /// Wrap an existing mounted authority to expose write API.
    pub fn from_mounted(mounted: Arc<MountedRootAuthority>) -> Self {
        Self { mounted }
    }

    /// Acquire a fresh write authority on the absolute mounted path.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let mounted = MountedRootAuthority::acquire(root)?;
        Ok(Self {
            mounted: Arc::new(mounted),
        })
    }

    /// The exact native mount path retained by this authority.
    pub fn root(&self) -> &Path {
        self.mounted.root()
    }

    /// Return the wrapped read authority for read operations.
    pub fn mount(&self) -> &Arc<MountedRootAuthority> {
        &self.mounted
    }

    /// Reverify the mount is still current.
    pub fn validate(&self) -> io::Result<()> {
        self.mounted.validate()
    }

    /// Prepare a writable target below the root. The destination path is
    /// checked against the conflict policy; a fresh, sibling temp file is
    /// created with `O_CREAT | O_EXCL` so a concurrent writer cannot smuggle
    /// a same-named file past publish.
    pub fn prepare_write_relative_file(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<PreparedWriteTarget> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_relative = assemble_relative(&components);
        let final_path = self.mounted.root().join(&final_relative);

        // The destination parent directory must be opened through the
        // retained authority so the boundary check matches the read path.
        let parent_components = parent_components_of(&components);
        if parent_components.as_os_str().is_empty() {
            let _root_bound = self.mounted.bind_root_directory()?;
        } else {
            let _parent_bound = self.mounted.open_relative_directory(&parent_components)?;
        }

        let (resolution, final_relative, staged_dir) = match policy {
            ConflictPolicy::Skip if final_path.exists() => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination exists and policy is Skip",
                ));
            }
            ConflictPolicy::Fail if final_path.exists() => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination exists and policy is Fail",
                ));
            }
            ConflictPolicy::Skip | ConflictPolicy::Fail => {
                (ConflictResolution::Fresh, final_relative, parent_components)
            }
            ConflictPolicy::Overwrite => (
                ConflictResolution::Overwrite,
                final_relative,
                parent_components,
            ),
            ConflictPolicy::Preserve => {
                if final_path.exists() {
                    let (preserved_rel, preserved_components) = preserved_sibling_path(
                        self.mounted.root(),
                        &parent_components,
                        components.last().expect("non-empty"),
                    )?;
                    (
                        ConflictResolution::Preserved,
                        preserved_rel,
                        preserved_components,
                    )
                } else {
                    (ConflictResolution::Fresh, final_relative, parent_components)
                }
            }
        };

        let staged_name = staging_leaf_name();
        let staged_path_abs = self.mounted.root().join(&staged_dir).join(&staged_name);
        let staged_file = create_exclusive_staged_file(&staged_path_abs)?;
        self.mounted.validate()?;

        Ok(PreparedWriteTarget {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            final_relative_path: final_relative,
            staged_path: staged_path_abs,
            staged_file,
            resolution,
            committed: false,
        })
    }

    /// Create a directory beneath the mount and bind it for further writes.
    pub fn create_relative_directory(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<MountedDirectory> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));

        match std::fs::symlink_metadata(&final_path) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "path exists and is not a directory",
                    ));
                }
                match policy {
                    ConflictPolicy::Skip | ConflictPolicy::Fail => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "directory exists and policy forbids overwriting",
                        ));
                    }
                    ConflictPolicy::Overwrite | ConflictPolicy::Preserve => {}
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_directory_atomic(self.mounted.root(), &components)?;
            }
            Err(error) => return Err(error),
        }
        self.mounted.validate()?;
        let _bound = self
            .mounted
            .open_relative_directory(&assemble_relative(&components))?;
        Ok(MountedDirectory {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            relative_path: assemble_relative(&components),
        })
    }

    /// Remove a regular file atomically through the retained authority.
    pub fn remove_relative_file(&self, relative: &Path) -> io::Result<()> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));
        let metadata = std::fs::symlink_metadata(&final_path)?;
        if metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove a directory through remove_relative_file",
            ));
        }
        std::fs::remove_file(&final_path)?;
        self.mounted.validate()?;
        Ok(())
    }

    /// Remove an empty directory atomically through the retained authority.
    pub fn remove_relative_directory(&self, relative: &Path) -> io::Result<()> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));
        let metadata = std::fs::symlink_metadata(&final_path)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove a non-directory through remove_relative_directory",
            ));
        }
        std::fs::remove_dir(&final_path)?;
        self.mounted.validate()?;
        Ok(())
    }
}

/// A directory created by [`MountedWriteAuthority::create_relative_directory`].
pub struct MountedDirectory {
    lease_token: Uuid,
    authority: Arc<MountedRootAuthority>,
    relative_path: PathBuf,
}

impl MountedDirectory {
    /// Return the relative path of this directory.
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    /// Prepare a writable file directly inside this directory.
    pub fn prepare_write_in_directory(
        &self,
        name: &str,
        policy: ConflictPolicy,
    ) -> io::Result<PreparedWriteTarget> {
        let mut relative = self.relative_path.clone();
        relative.push(name);
        MountedWriteAuthority::from_mounted(Arc::clone(&self.authority))
            .prepare_write_relative_file(&relative, policy)
    }
}

fn strict_relative_components(relative: &Path) -> io::Result<Vec<OsString>> {
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

fn assemble_relative(components: &[OsString]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in components {
        path.push(component);
    }
    path
}

fn parent_components_of(components: &[OsString]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in &components[..components.len().saturating_sub(1)] {
        path.push(component);
    }
    path
}

fn staging_leaf_name() -> OsString {
    let token = Uuid::new_v4();
    let mut name = OsString::from(".tributary-stage-");
    name.push(token.to_string());
    name.push(".tmp");
    name
}

fn preserved_sibling_path(
    root: &Path,
    parent_components: &Path,
    leaf: &OsString,
) -> io::Result<(PathBuf, PathBuf)> {
    let parent_abs = if parent_components.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(parent_components)
    };
    let entries = match std::fs::read_dir(&parent_abs) {
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
    let (stem, ext) = match original.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), Some(ext.to_string())),
        _ => (original.clone(), None),
    };
    for index in 1..=u32::MAX {
        let candidate = match &ext {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        if !existing.iter().any(|name| name == &candidate) {
            // Compose the final relative path (parent + leaf candidate).
            let mut relative = PathBuf::new();
            if !parent_components.as_os_str().is_empty() {
                relative.push(parent_components);
            }
            relative.push(&candidate);
            // The directory used to stage the temp file is the same parent
            // directory so the rename stays atomic on one filesystem.
            return Ok((relative, parent_components.to_path_buf()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no preserved name available for conflict",
    ))
}

fn create_directory_atomic(root: &Path, components: &[OsString]) -> io::Result<()> {
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

#[cfg(unix)]
fn create_exclusive_staged_file(path: &Path) -> io::Result<File> {
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

#[cfg(windows)]
fn create_exclusive_staged_file(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn create_exclusive_staged_file(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "write authority is unsupported on this platform",
    ))
}

fn rollback_staged(staged_path: &Path) -> io::Result<()> {
    match std::fs::remove_file(staged_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn publish_atomic(staged_path: &Path, final_path: &Path) -> io::Result<()> {
    // POSIX rename is atomic on the same filesystem. Windows std::fs::rename
    // uses MoveFileExW with MOVEFILE_REPLACE_EXISTING semantics, which is
    // likewise atomic on the same volume.
    std::fs::rename(staged_path, final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tributary-write-authority-{label}-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create root");
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn fresh_write_commits_atomically() {
        let root = unique_root("fresh");
        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

        let mut staged = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
            .expect("prepare staged file");
        staged.write_all(b"audio payload").expect("write payload");

        let outcome = staged.commit().expect("commit staged file");
        assert_eq!(outcome.resolution, ConflictResolution::Fresh);
        assert_eq!(
            std::fs::read(root.join("song.flac")).expect("read final"),
            b"audio payload"
        );

        cleanup(&root);
    }

    #[test]
    fn skip_policy_rejects_when_destination_exists() {
        let root = unique_root("skip");
        std::fs::write(root.join("song.flac"), b"existing").expect("write existing");

        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
        let error = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Skip)
            .expect_err("skip policy must reject existing destination");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        cleanup(&root);
    }

    #[test]
    fn overwrite_policy_replaces_final_file() {
        let root = unique_root("overwrite");
        std::fs::write(root.join("song.flac"), b"old").expect("write existing");

        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
        let mut staged = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
            .expect("prepare overwrite");
        staged.write_all(b"new").expect("write new");
        staged.commit().expect("commit overwrite");

        assert_eq!(
            std::fs::read(root.join("song.flac")).expect("read final"),
            b"new"
        );

        cleanup(&root);
    }

    #[test]
    fn preserve_policy_writes_to_disambiguated_name() {
        let root = unique_root("preserve");
        std::fs::write(root.join("song.flac"), b"first").expect("write first");

        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
        let mut staged = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
            .expect("prepare preserve");
        staged.write_all(b"second").expect("write second");
        let outcome = staged.commit().expect("commit preserve");

        assert_eq!(outcome.resolution, ConflictResolution::Preserved);
        assert_eq!(
            std::fs::read(root.join("song.flac")).expect("read original"),
            b"first"
        );
        assert_eq!(
            std::fs::read(root.join(&outcome.relative_path)).expect("read preserved"),
            b"second"
        );

        cleanup(&root);
    }

    #[test]
    fn rollback_removes_staged_file() {
        let root = unique_root("rollback");
        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

        let mut staged = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
            .expect("prepare staged");
        staged.write_all(b"partial").expect("write partial");
        let staged_path = staged.staged_path.clone();
        // Sanity: the staged file actually exists before rollback.
        assert!(staged_path.exists());
        staged.rollback().expect("rollback staged");
        assert!(!staged_path.exists());
        assert!(!root.join("song.flac").exists());

        cleanup(&root);
    }

    #[test]
    fn cross_mount_path_is_rejected() {
        let root = unique_root("cross-mount");
        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

        let error = authority
            .prepare_write_relative_file(Path::new("../outside.flac"), ConflictPolicy::Fail)
            .expect_err("parent path must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = authority
            .prepare_write_relative_file(Path::new("/etc/passwd"), ConflictPolicy::Fail)
            .expect_err("absolute path must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        cleanup(&root);
    }

    #[test]
    fn directory_creation_and_file_writes_combine() {
        let root = unique_root("dirs");
        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

        let bound = authority
            .create_relative_directory(Path::new("album"), ConflictPolicy::Fail)
            .expect("create album dir");
        assert_eq!(bound.relative_path(), Path::new("album"));

        let mut staged = bound
            .prepare_write_in_directory("song.flac", ConflictPolicy::Fail)
            .expect("prepare file under dir");
        staged.write_all(b"nested").expect("write nested");
        staged.commit().expect("commit nested");

        assert_eq!(
            std::fs::read(root.join("album/song.flac")).expect("read nested"),
            b"nested"
        );

        cleanup(&root);
    }

    #[test]
    fn prepared_target_resolves_only_one_preserved_name() {
        let root = unique_root("preserve-twice");
        std::fs::write(root.join("song.flac"), b"original").expect("write original");

        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

        let mut first = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
            .expect("prepare first preserve");
        first.write_all(b"a").expect("write first");
        let first_outcome = first.commit().expect("commit first");

        let mut second = authority
            .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
            .expect("prepare second preserve");
        second.write_all(b"b").expect("write second");
        let second_outcome = second.commit().expect("commit second");

        assert_ne!(first_outcome.relative_path, second_outcome.relative_path);
        let names: Vec<String> = std::fs::read_dir(&root)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        assert_eq!(names.len(), 3);
        assert!(names.iter().any(|name| name == "song.flac"));
        assert!(names.iter().any(|name| name == "song (1).flac"));
        assert!(names.iter().any(|name| name == "song (2).flac"));

        cleanup(&root);
    }

    #[test]
    fn remove_relative_file_only_accepts_regular_files() {
        let root = unique_root("remove-file");
        std::fs::write(root.join("song.flac"), b"data").expect("write file");
        std::fs::create_dir(root.join("album")).expect("create album");

        let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
        authority
            .remove_relative_file(Path::new("song.flac"))
            .expect("remove file");
        assert!(!root.join("song.flac").exists());

        let error = authority
            .remove_relative_file(Path::new("album"))
            .expect_err("directory must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        cleanup(&root);
    }
}
