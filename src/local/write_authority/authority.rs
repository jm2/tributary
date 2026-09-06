//! The [`MountedWriteAuthority`] type: retained, validated write operations
//! beneath one exact mounted filesystem.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::policy::{ConflictPolicy, ConflictResolution};
use super::staging::{
    assemble_relative, bind_publish_parent, create_directory_atomic, create_exclusive_staged_file,
    parent_components_of, preserved_sibling_path, rename_bounded, staging_leaf_name,
    strict_relative_components,
};
use super::target::{parent_relative_of, MountedDirectory, PreparedWriteTarget};
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
    ///
    /// The policy is resolved against the live filesystem here (through
    /// `symlink_metadata`, so a dangling symlink counts as an existing
    /// destination and is never replaced through), and the resolved decision
    /// then drives the staged write exactly as an externally planned
    /// resolution would.
    pub fn prepare_write_relative_file(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<PreparedWriteTarget> {
        let resolution = self.resolve_policy(relative, policy)?;
        self.prepare_write_with_resolution(relative, resolution)
    }

    /// Observe the live filesystem and resolve one conflict policy to the
    /// concrete resolution a staged write will carry.
    fn resolve_policy(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<ConflictResolution> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        bind_parent_directory(&self.mounted, &components)?;
        let exists = std::fs::symlink_metadata(
            self.mounted
                .root()
                .join(assemble_relative(&components)),
        )
        .is_ok();
        match (policy, exists) {
            (ConflictPolicy::Skip, true) | (ConflictPolicy::Fail, true) => {
                Err(destination_exists_error(match policy {
                    ConflictPolicy::Skip => "Skip",
                    _ => "Fail",
                }))
            }
            (ConflictPolicy::Overwrite, true) => Ok(ConflictResolution::Overwrite),
            (ConflictPolicy::Preserve, true) => Ok(ConflictResolution::Preserved),
            (_, false) => Ok(ConflictResolution::Fresh),
        }
    }

    /// Prepare a writable target that carries a decision already made at
    /// plan time. The executor consumes the planned resolution through this
    /// entry point and never re-decides a conflict.
    ///
    /// `Fresh` stages the requested path and publishes with atomic
    /// no-replace semantics: a destination that appears after planning makes
    /// the commit fail with `AlreadyExists` so the caller can represent the
    /// post-plan skip distinctly. `Overwrite` saves any pre-existing
    /// destination aside (see [`PreparedWriteTarget::commit`]) and replaces
    /// it. `Preserved` disambiguates a non-colliding sibling at prepare time
    /// against the live directory and publishes no-replace to that name; the
    /// [`CommitOutcome`](super::CommitOutcome) reports the actual published
    /// path.
    pub fn prepare_write_with_resolution(
        &self,
        relative: &Path,
        resolution: ConflictResolution,
    ) -> io::Result<PreparedWriteTarget> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        bind_parent_directory(&self.mounted, &components)?;

        let (final_relative, staged_dir) = if resolution == ConflictResolution::Preserved {
            preserved_sibling_path(
                self.mounted.root(),
                &parent_components_of(&components),
                components.last().expect("strict components are non-empty"),
            )?
        } else {
            (
                assemble_relative(&components),
                parent_components_of(&components),
            )
        };

        let staged_name = staging_leaf_name();
        let staged_path = self
            .mounted
            .root()
            .join(&staged_dir)
            .join(&staged_name);
        let staged_file = create_exclusive_staged_file(&staged_path)?;
        self.mounted.validate()?;

        Ok(PreparedWriteTarget {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            final_relative_path: final_relative,
            staged_path,
            staged_file: Some(staged_file),
            resolution,
            no_replace: resolution != ConflictResolution::Overwrite,
            committed: false,
        })
    }

    /// Restore an original saved aside by an Overwrite commit back over the
    /// published file. Transfer rollback uses this so a failed transfer
    /// leaves the mount exactly as it found it: the saved original replaces
    /// the file this transfer published, and nothing the transfer did not
    /// create is ever deleted. The backup must be a sibling of the published
    /// file, which is how [`PreparedWriteTarget::commit`] creates it.
    pub fn restore_overwritten_file(
        &self,
        published_relative: &Path,
        backup_relative: &Path,
    ) -> io::Result<()> {
        let published_components = strict_relative_components(published_relative)?;
        let backup_components = strict_relative_components(backup_relative)?;
        let backup_parent = parent_components_of(&backup_components);
        if backup_parent != parent_components_of(&published_components) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "overwrite backup must be a sibling of the published file",
            ));
        }
        self.mounted.validate()?;
        let parent = bind_publish_parent(&self.mounted, &parent_relative_of(backup_relative))?;
        rename_bounded(
            &parent,
            backup_components.last().expect("strict components are non-empty"),
            published_components
                .last()
                .expect("strict components are non-empty"),
            false,
        )?;
        self.mounted.validate()?;
        Ok(())
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

/// The `AlreadyExists` error raised when a Skip/Fail policy meets an
/// existing destination.
fn destination_exists_error(policy: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("destination exists and policy is {policy}"),
    )
}
