//! The [`MountedWriteAuthority`] type: retained, validated write operations
//! beneath one exact mounted filesystem.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::policy::{ConflictPolicy, ConflictResolution};
use super::staging::{
    assemble_relative, create_directory_atomic, create_exclusive_staged_file, parent_components_of,
    preserved_sibling_path, staging_leaf_name, strict_relative_components,
};
use super::target::{MountedDirectory, PreparedWriteTarget};
use crate::local::root_authority::MountedRootAuthority;

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

/// The policy-resolved destination of a staged write: the resolution
/// recorded on the target, the final relative path the staged file will be
/// renamed to, and the directory the staged file is created in.
struct ResolvedDestination {
    resolution: ConflictResolution,
    final_relative: PathBuf,
    staged_dir: PathBuf,
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
        bind_parent_directory(&self.mounted, &components)?;

        let resolved = resolve_write_destination(
            self.mounted.root(),
            &components,
            assemble_relative(&components),
            policy,
        )?;

        let staged_name = staging_leaf_name();
        let staged_path = self
            .mounted
            .root()
            .join(&resolved.staged_dir)
            .join(&staged_name);
        let staged_file = create_exclusive_staged_file(&staged_path)?;
        self.mounted.validate()?;

        Ok(PreparedWriteTarget {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            final_relative_path: resolved.final_relative,
            staged_path,
            staged_file: Some(staged_file),
            resolution: resolved.resolution,
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

/// Bind the destination parent directory through the retained authority so
/// the boundary check matches the read path. A write directly beneath the
/// root binds the root itself.
fn bind_parent_directory(
    mounted: &MountedRootAuthority,
    components: &[OsString],
) -> io::Result<()> {
    let parent = parent_components_of(components);
    if parent.as_os_str().is_empty() {
        let _root_bound = mounted.bind_root_directory()?;
    } else {
        let _parent_bound = mounted.open_relative_directory(&parent)?;
    }
    Ok(())
}

/// Resolve the conflict policy against the live filesystem and decide where
/// the staged file is created and what it is finally named.
fn resolve_write_destination(
    root: &Path,
    components: &[OsString],
    final_relative: PathBuf,
    policy: ConflictPolicy,
) -> io::Result<ResolvedDestination> {
    let final_path = root.join(&final_relative);
    let parent_components = parent_components_of(components);
    match policy {
        ConflictPolicy::Skip if final_path.exists() => Err(destination_exists_error("Skip")),
        ConflictPolicy::Fail if final_path.exists() => Err(destination_exists_error("Fail")),
        ConflictPolicy::Skip | ConflictPolicy::Fail => Ok(ResolvedDestination {
            resolution: ConflictResolution::Fresh,
            final_relative,
            staged_dir: parent_components,
        }),
        ConflictPolicy::Overwrite => Ok(ResolvedDestination {
            resolution: ConflictResolution::Overwrite,
            final_relative,
            staged_dir: parent_components,
        }),
        ConflictPolicy::Preserve if final_path.exists() => {
            let (preserved_relative, preserved_components) = preserved_sibling_path(
                root,
                &parent_components,
                components.last().expect("non-empty"),
            )?;
            Ok(ResolvedDestination {
                resolution: ConflictResolution::Preserved,
                final_relative: preserved_relative,
                staged_dir: preserved_components,
            })
        }
        ConflictPolicy::Preserve => Ok(ResolvedDestination {
            resolution: ConflictResolution::Fresh,
            final_relative,
            staged_dir: parent_components,
        }),
    }
}

/// The `AlreadyExists` error raised when a Skip/Fail policy meets an
/// existing destination.
fn destination_exists_error(policy: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("destination exists and policy is {policy}"),
    )
}
