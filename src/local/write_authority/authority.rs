//! The [`MountedWriteAuthority`] type: retained, validated write operations
//! beneath one exact mounted filesystem.

use std::cell::Cell;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::policy::{ConflictPolicy, ConflictResolution};
use super::staging::{
    assemble_relative, create_exclusive_staged_file, parent_components_of, preserved_sibling_path,
    staging_leaf_name, strict_relative_components,
};
use super::target::{MountedDirectory, PreparedWriteTarget};
use crate::local::root_authority::{
    LeafIdentity, MountedRootAuthority, RestoreSlot, RetainedWriteParent, ReversalOutcome,
};
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
    ///
    /// The parent directory is retained for the lifetime of the target and
    /// every later operation — staged creation, publish, and discard — is
    /// anchored to that retained handle (or revalidated against it), so a
    /// parent or mount replacement between staging and commit cannot
    /// redirect a write.
    pub fn prepare_write_relative_file(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<PreparedWriteTarget> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;

        let parent_components = parent_components_of(&components);
        let parent = self.mounted.retain_write_parent(&parent_components)?;

        let resolved = resolve_write_destination(
            self.mounted.root(),
            &components,
            assemble_relative(&components),
            policy,
        )?;
        self.stage_into_resolved_destination(parent, resolved)
    }

    /// Prepare a writable target whose conflict outcome the planner already
    /// recorded on the stage. The recorded resolution is consumed verbatim:
    ///
    /// - [`ConflictResolution::Fresh`] publishes no-replace: a destination
    ///   that appears after planning fails the commit instead of being
    ///   replaced.
    /// - [`ConflictResolution::Overwrite`] publishes by replace with a
    ///   commit-time backup bind (see
    ///   [`PreparedWriteTarget::commit`]); an occupant that appears or
    ///   vanishes after planning is backed up or bypassed, never destroyed.
    /// - [`ConflictResolution::Preserved`] allocates the disambiguated
    ///   sibling name now and publishes it no-replace.
    ///
    /// The live filesystem is never consulted to re-decide a recorded
    /// resolution — only to allocate a Preserved sibling name — so the
    /// executor can never flip a decision the planner made.
    pub fn prepare_write_relative_file_with_resolution(
        &self,
        relative: &Path,
        resolution: ConflictResolution,
    ) -> io::Result<PreparedWriteTarget> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;

        let parent_components = parent_components_of(&components);
        let parent = self.mounted.retain_write_parent(&parent_components)?;

        let resolved = destination_for_resolution(
            self.mounted.root(),
            &components,
            assemble_relative(&components),
            resolution,
        )?;
        self.stage_into_resolved_destination(parent, resolved)
    }

    /// Create the exclusive staged file inside the resolved destination and
    /// bundle it into the prepared target. Shared tail of both prepare
    /// entry points.
    fn stage_into_resolved_destination(
        &self,
        parent: RetainedWriteParent,
        resolved: ResolvedDestination,
    ) -> io::Result<PreparedWriteTarget> {
        let staged_name = staging_leaf_name();
        let staged_path = self
            .mounted
            .root()
            .join(&resolved.staged_dir)
            .join(&staged_name);
        #[cfg(unix)]
        let staged_file = create_exclusive_staged_file(parent.handle(), staged_name.as_os_str())?;
        #[cfg(windows)]
        let staged_file = create_exclusive_staged_file(&staged_path)?;
        #[cfg(not(any(unix, windows)))]
        let staged_file = create_exclusive_staged_file(&staged_path)?;
        self.mounted.validate()?;

        Ok(PreparedWriteTarget {
            lease_token: self.mounted.token(),
            authority: Arc::clone(&self.mounted),
            parent,
            final_relative_path: resolved.final_relative,
            staged_leaf: staged_name,
            staged_path,
            staged_file: Some(staged_file),
            resolution: resolved.resolution,
            committed: false,
            staged_leaf_holds_displaced_occupant: Cell::new(false),
        })
    }

    /// Create a directory beneath the mount and bind it for further writes.
    ///
    /// Returns the bound directory together with a report of the components
    /// this call actually created, leaf inclusive (empty when the leaf
    /// already existed and was adopted). Ownership recording for rollback
    /// MUST use this list, including the per-component identities captured
    /// during the exclusive creation: a component that already existed —
    /// including one a concurrent writer created moments before the
    /// creation call — is adopted, not owned, and must survive rollback,
    /// and a component replaced after its creation must never be recorded
    /// under the newcomer's identity.
    pub fn create_relative_directory(
        &self,
        relative: &Path,
        policy: ConflictPolicy,
    ) -> io::Result<(MountedDirectory, Vec<CreatedDirectoryEntry>)> {
        let components = strict_relative_components(relative)?;
        self.mounted.validate()?;
        let final_path = self.mounted.root().join(assemble_relative(&components));

        let created = match std::fs::symlink_metadata(&final_path) {
            Ok(metadata) => {
                adopt_existing_directory(metadata, policy)?;
                Vec::new()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_missing_directory(&self.mounted, &components)?
            }
            Err(error) => return Err(error),
        };
        self.mounted.validate()?;
        let _bound = self
            .mounted
            .open_relative_directory(&assemble_relative(&components))?;
        Ok((
            MountedDirectory {
                lease_token: self.mounted.token(),
                authority: Arc::clone(&self.mounted),
                relative_path: assemble_relative(&components),
                identity: self.mounted.relative_leaf_identity(relative),
            },
            created,
        ))
    }

    /// Remove a regular file atomically through the retained authority.
    ///
    /// On Unix the removal is anchored to the retained root handle: the
    /// parent is walked no-follow from the retained root and the leaf is
    /// unlinked through its descriptor, so an intermediate symlink or a
    /// replaced directory cannot redirect the removal.
    pub fn remove_relative_file(&self, relative: &Path) -> io::Result<()> {
        strict_relative_components(relative)?;
        self.mounted.remove_regular_file_within(relative)
    }

    /// Remove an empty directory atomically through the retained authority,
    /// anchored to the retained root exactly like
    /// [`Self::remove_relative_file`].
    pub fn remove_relative_directory(&self, relative: &Path) -> io::Result<()> {
        strict_relative_components(relative)?;
        self.mounted.remove_directory_within(relative)
    }

    /// Restore a previously saved backup over its destination within the
    /// same parent directory, replacing whatever currently occupies the
    /// destination name.
    ///
    /// Used by the transfer executor's rollback to put a pre-existing
    /// original back after an Overwrite commit. The rename is anchored to
    /// the retained root/parent handles on Unix; the backup and the
    /// destination must be siblings so one retained parent directory serves
    /// both sides of the rename.
    pub fn restore_relative_file(
        &self,
        backup_relative: &Path,
        destination_relative: &Path,
    ) -> io::Result<()> {
        self.restore_relative_file_verified(backup_relative, destination_relative, None, None)
            .map(|_| ())
    }

    /// Restore a previously saved backup over its destination within the
    /// same parent directory, verifying against the published-leaf identity
    /// when one is supplied.
    ///
    /// A destination that still names the transfer's publication — or any
    /// object, when no identity was recorded at publish time — is replaced
    /// by the backup: that slot is the transfer's own. An absent
    /// destination is restored anyway: the publication was deleted by a
    /// concurrent writer, the slot is empty, and putting the transfer's
    /// original back returns the destination to its pre-transfer state. A
    /// destination occupied by a *different* object is refused with
    /// [`ReversalOutcome::RefusedForeignLeaf`] and left untouched — a
    /// concurrent writer owns that name now, and rolling back over it
    /// would destroy a file the transfer does not own.
    ///
    /// The backup itself is verified against `backup_leaf_identity` — the
    /// replaced occupant's bind-time identity recorded on the commit
    /// outcome — before it is moved, compared change-instant-exact: the
    /// bind captures the object's identity after the linking operation
    /// has updated its instant, so a legitimate backup compares exactly
    /// equal, and a same-index swap-in of a foreign object — which cannot
    /// inherit the recorded instant — is refused. A swapped backup is
    /// reported as
    /// [`ReversalOutcome::RefusedForeignLeaf`]: the replacement is never
    /// installed as the original and never deleted. `None` degrades to the
    /// legacy uncoupled handling, exactly like every other
    /// uncaptured-identity reversal.
    pub fn restore_relative_file_verified(
        &self,
        backup_relative: &Path,
        destination_relative: &Path,
        expected: Option<&LeafIdentity>,
        backup_leaf_identity: Option<&LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        self.mounted.restore_relative_file_verified(
            backup_relative,
            destination_relative,
            expected,
            backup_leaf_identity,
            RestoreSlot::IdentityCoupled,
        )
    }

    /// Restore a displaced-only backup into an ABSENT destination slot,
    /// refusing any occupant.
    ///
    /// The counterpart of a Windows replace publish that exhausted its
    /// rebind bound: nothing of the transfer's ever landed, and the
    /// retained backup holds the displaced pre-transfer occupant. Because
    /// no publication identity exists to couple a replacement, the backup
    /// is installed only with no-replace semantics and any occupant is
    /// refused intact. The backup is verified against its bind-time
    /// identity and never deleted on refusal or failure, so the displaced
    /// original survives for a later, unblocked restore.
    pub fn restore_displaced_only_verified(
        &self,
        backup_relative: &Path,
        destination_relative: &Path,
        backup_leaf_identity: Option<&LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        self.mounted.restore_displaced_only_verified(
            backup_relative,
            destination_relative,
            backup_leaf_identity,
        )
    }

    /// Discard the saved original of a successful overwrite commit,
    /// verifying the backup still names its bind-time object — object-
    /// coupled — before it is removed. See
    /// [`BoundOccupantBackup`](crate::local::root_authority::BoundOccupantBackup).
    /// A swapped backup is refused and surfaced, never deleted.
    pub fn discard_backup_relative_file(
        &self,
        backup_relative: &Path,
        backup_leaf_identity: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        self.mounted
            .discard_backup_within(backup_relative, backup_leaf_identity)
    }

    /// Remove a regular file atomically through the retained authority,
    /// verifying against the publish-time leaf identity when one is
    /// supplied. See [`ReversalOutcome`] for the outcome semantics.
    pub fn remove_relative_file_verified(
        &self,
        relative: &Path,
        expected: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        self.mounted
            .remove_regular_file_within_verified(relative, expected)
    }

    /// Remove an empty directory atomically through the retained authority,
    /// anchored to the retained root exactly like
    /// [`Self::remove_relative_file`], verifying against the creation-time
    /// leaf identity when one is supplied. See [`ReversalOutcome`] for the
    /// outcome semantics.
    pub fn remove_relative_directory_verified(
        &self,
        relative: &Path,
        expected: Option<LeafIdentity>,
    ) -> io::Result<ReversalOutcome> {
        self.mounted
            .remove_directory_within_verified(relative, expected)
    }

    /// Best-effort no-follow identity of the leaf at `relative` beneath the
    /// retained root. `None` when the leaf is absent or the platform cannot
    /// capture an identity. Used by the transfer executor to record freshly
    /// created directories (and any ancestors it had to create) for
    /// identity-verified rollback.
    pub fn relative_leaf_identity(&self, relative: &Path) -> Option<LeafIdentity> {
        self.mounted.relative_leaf_identity(relative)
    }

    /// Whether `relative` names a leaf beneath the retained destination root,
    /// probed with per-component no-follow traversal exactly as execution
    /// traverses the destination. `Ok(false)` means the leaf is absent (the
    /// leaf or one of its parents is missing); a symlink/reparse-point
    /// ancestor, a non-directory parent, a boundary crossing, or any other
    /// traversal failure is an error. Callers classifying a destination
    /// conflict MUST use this rather than an absolute-path `symlink_metadata`,
    /// which follows an ancestor symlink outside the retained authority.
    pub fn relative_leaf_exists(&self, relative: &Path) -> io::Result<bool> {
        self.mounted.relative_leaf_exists(relative)
    }
}

/// One directory component a creation call actually created, paired with
/// the no-follow identity captured from the created object itself during
/// the exclusive creation. Ownership recording MUST use this report: the
/// identity names the object the creation call produced, so a concurrent
/// writer that replaces a just-created component before the ownership
/// record is written is never matched — and never destroyed — by the
/// eventual identity-verified rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedDirectoryEntry {
    /// Relative path of the created component, leaf inclusive.
    pub relative_path: PathBuf,
    /// Identity of the created object, captured during the exclusive
    /// creation from the creation-time handle. `None` only when the
    /// platform cannot capture an identity; such an entry degrades the
    /// eventual reversal to the legacy path-only behavior.
    pub identity: Option<LeafIdentity>,
}

/// Reconcile an existing destination directory with the conflict policy:
/// the entry must be a real directory, and Skip/Fail reject it outright
/// while Overwrite/Preserve adopt it as-is.
fn adopt_existing_directory(metadata: std::fs::Metadata, policy: ConflictPolicy) -> io::Result<()> {
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "path exists and is not a directory",
        ));
    }
    match policy {
        ConflictPolicy::Skip | ConflictPolicy::Fail => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "directory exists and policy forbids overwriting",
        )),
        ConflictPolicy::Overwrite | ConflictPolicy::Preserve => Ok(()),
    }
}

/// Create every component of a not-yet-existing directory path through the
/// retained authority: walked and created no-follow from the retained root
/// handle on Unix, per-component creation with an object-anchored identity
/// capture elsewhere. Returns the components this invocation actually
/// created, leaf inclusive, each with the identity captured during its
/// exclusive creation — an adopted (already existing) component is not
/// reported, so ownership recording never claims a directory the transfer
/// did not create, and no reported identity is read from a lookup a later
/// replacement could influence.
fn create_missing_directory(
    authority: &MountedRootAuthority,
    components: &[OsString],
) -> io::Result<Vec<CreatedDirectoryEntry>> {
    let created = authority.create_directories_within(components)?;
    Ok(created
        .into_iter()
        .map(|component| CreatedDirectoryEntry {
            relative_path: assemble_relative(&components[..=component.index]),
            identity: component.identity,
        })
        .collect())
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

/// Allocate the staged-write destination for a planner-recorded resolution.
/// Unlike [`resolve_write_destination`] this never consults the conflict
/// policy or re-decides an outcome: Fresh and Overwrite keep the final
/// name, and Preserved only allocates its disambiguated sibling name.
fn destination_for_resolution(
    root: &Path,
    components: &[OsString],
    final_relative: PathBuf,
    resolution: ConflictResolution,
) -> io::Result<ResolvedDestination> {
    match resolution {
        ConflictResolution::Fresh | ConflictResolution::Overwrite => Ok(ResolvedDestination {
            resolution,
            final_relative,
            staged_dir: parent_components_of(components),
        }),
        ConflictResolution::Preserved => {
            let (preserved_relative, preserved_components) = preserved_sibling_path(
                root,
                &parent_components_of(components),
                components.last().expect("non-empty"),
            )?;
            Ok(ResolvedDestination {
                resolution: ConflictResolution::Preserved,
                final_relative: preserved_relative,
                staged_dir: preserved_components,
            })
        }
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
