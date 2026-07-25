//! Root-relative folder-browsing index.
//!
//! Tributary's local library is rooted at one or more configured filesystem
//! directories. The folder browser renders those roots as the top of a lazy
//! directory tree, expands them into subdirectories on demand, and reports
//! track counts at every level.
//!
//! Design contract (see `docs/task.md` § P2.3 — root-relative folder browsing,
//! issue #14):
//!
//! * **Root-relative.** Folder paths are always expressed relative to a single
//!   configured root; the absolute path is never user-visible inside a row.
//! * **Multi-root disambiguation.** When the configured roots overlap (one is
//!   a parent of another, or two roots share a subtree), every track is owned
//!   by exactly one root — the most specific (deepest) configured ancestor.
//!   Lazy expansion of a root lists its immediate children, never another
//!   root's contents, even when a parent root is also configured.
//! * **Lazy navigation.** Nothing beyond the immediate child directories of an
//!   expanded node is read from disk; opening a node calls into the index
//!   with the requested directory and walks the filesystem only at that
//!   level. Closing a node drops no state because nothing was cached.
//! * **Unavailable / renamed roots.** Roots persisted as `is_available = false`
//!   or whose configured path no longer matches any filesystem directory are
//!   surfaced as explicit placeholder rows. A persisted `library_root` row
//!   whose path no longer matches any currently configured path appears as
//!   an additional placeholder so the user can see which library entries fell
//!   off between sessions.
//! * **Explicit omission policy for pathless sources.** Tracks with no
//!   `file_path` (server backends, external files, removable scan-only rows)
//!   are deliberately omitted from the folder browser. The pane shows a
//!   documented placeholder so users understand that the absence is by
//!   design, not a bug.
//! * **Containment.** `child_dirs` and `dir_tracks` refuse to escape a root:
//!   any caller-supplied path that resolves above the configured root via
//!   `..` components is rejected before the filesystem is touched.

// Pattern names read more clearly than `Self::` here. The clippy lint is
// disabled per-item below rather than at the crate root so the suppression
// stays scoped to the new code.
// `clippy::pedantic` enables `use_self`; this module deliberately spells
// variant names in full to keep match arms aligned with the enum
// definition. The lint applies module-wide via a scoped `#![allow(...)]`
// below — modules accept inner attributes the same way the binary does.
#![allow(clippy::use_self)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::db::entities::library_root;

/// What the folder browser shows for a configured root at its top level.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RootEntry {
    /// A root that is configured, persisted as available, and readable on disk.
    Available(RootSummary),
    /// A root whose configured path could not be read at the most recent
    /// reconciliation. The original path is shown so the user knows which
    /// library entry failed.
    Unavailable {
        configured_path: PathBuf,
        reason: UnavailableReason,
    },
}

impl RootEntry {
    pub fn configured_path(&self) -> &Path {
        match self {
            Self::Available(summary) => summary.configured_path.as_path(),
            Self::Unavailable { configured_path, .. } => configured_path.as_path(),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
}

/// Available root summary surfaced at the top of the browser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootSummary {
    pub configured_path: PathBuf,
    /// Number of persisted tracks that live directly under this root.
    pub direct_track_count: usize,
    /// Number of immediate subdirectories under this root that contain at
    /// least one persisted track (not every filesystem child).
    pub child_dir_count: usize,
}

/// Why a configured root is reported as unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    /// The configured path no longer points at any filesystem directory.
    Missing,
    /// The persisted `library_root` row records `is_available = false`.
    PersistedUnavailable,
    /// The persisted row has not completed an authoritative scan.
    ScanIncomplete,
}

/// One immediate child of an expanded folder node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// Folder name to display (never the full path).
    pub name: String,
    /// Path relative to the owning root, using `/` separators for display.
    pub root_relative: String,
    /// Number of persisted tracks directly inside this directory (not below).
    pub direct_track_count: usize,
    /// Whether this directory has any subdirectories containing tracks.
    pub has_descendants: bool,
}

/// What the folder browser says when there is nothing to show.
///
/// Distinct from "empty roots" — empty roots still display as a configured
/// root with `direct_track_count = 0`. The omission cases name the missing
/// preconditions explicitly so the UI can match each one with copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OmissionNotice {
    /// No library roots are configured at all (fresh profile).
    NoRootsConfigured,
    /// Library roots are configured, but every persisted track in the current
    /// snapshot sits outside the configured roots (e.g. a previously trusted
    /// removable volume was unmounted, or root reauthorization is pending).
    NoTracksUnderRoots,
}

/// One track position inside the folder browser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackRef {
    /// Zero-based position of the track in the supplied slice.
    pub index: usize,
    /// Root-relative path of the directory that owns the track.
    pub root_relative: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
struct ChildTrackStats {
    direct: usize,
    has_descendants: bool,
}

/// Pure-data index over the configured roots and a slice of track paths.
///
/// Construct once per browser refresh; the resulting struct is cheap to clone
/// and shares nothing with callers. All filesystem reads happen behind
/// [`Self::child_dirs`] and [`Self::dir_tracks`] — callers expand folders on
/// demand.
#[derive(Clone, Debug)]
pub struct FolderIndex {
    roots: Vec<RootEntry>,
    /// Track slice passed to the constructor.
    track_paths: Vec<Option<PathBuf>>,
    /// For every available root, the set of root-relative paths to every
    /// subdirectory that holds at least one track, plus the number of tracks
    /// inside each directory (not its descendants).
    root_subdirs: BTreeMap<PathBuf, BTreeMap<PathBuf, ChildTrackStats>>,
    /// Tracks per root, keyed by root-relative directory path.
    root_tracks: BTreeMap<PathBuf, Vec<usize>>,
    /// Tracks whose path is not under any configured root. Held so the pane
    /// can show the documented omission notice instead of silently dropping
    /// them.
    unrooted_track_count: usize,
}

impl FolderIndex {
    /// Build the index.
    ///
    /// `configured_paths` is the active set of library roots from
    /// `AppConfig::library_paths`. `persisted_roots` is the latest
    /// `library_roots` table contents read at startup; pass an empty slice
    /// when the table has not been populated yet. `track_paths` is the
    /// full-library slice the caller is about to render.
    pub fn build<I, P>(
        configured_paths: I,
        persisted_roots: &[library_root::Model],
        track_paths: P,
    ) -> Self
    where
        I: IntoIterator<Item = PathBuf>,
        P: IntoIterator<Item = Option<PathBuf>>,
    {
        let configured_paths: Vec<PathBuf> = configured_paths.into_iter().collect();
        let track_paths: Vec<Option<PathBuf>> = track_paths.into_iter().collect();

        let mut index = Self {
            roots: Vec::with_capacity(configured_paths.len()),
            root_subdirs: BTreeMap::new(),
            root_tracks: BTreeMap::new(),
            track_paths: track_paths.clone(),
            unrooted_track_count: 0,
        };

        // Sort roots deepest-first so each track's most-specific ancestor
        // wins. Dedup is exact (case-sensitive on Unix, case-insensitive on
        // Windows via Path equality semantics).
        let mut sorted_roots = configured_paths.clone();
        sorted_roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
        sorted_roots.dedup();

        let persisted_by_path: BTreeMap<&str, &library_root::Model> = persisted_roots
            .iter()
            .map(|model| (model.path.as_str(), model))
            .collect();

        for configured in &sorted_roots {
            let entry = classify_root(configured, &persisted_by_path);
            index.roots.push(entry);
        }

        // Surface persisted rows that no longer correspond to any configured
        // path as additional unavailable placeholders so the user can see
        // which library entries fell off between sessions.
        for persisted in persisted_roots {
            let persisted_path = PathBuf::from(&persisted.path);
            if configured_paths.iter().any(|c| c == &persisted_path) {
                continue;
            }
            let reason = if !persisted.is_available {
                UnavailableReason::PersistedUnavailable
            } else if !persisted.last_scan_complete {
                UnavailableReason::ScanIncomplete
            } else {
                // The persisted row still claims to be available, but the
                // user removed it from preferences before the engine
                // committed the change. Surface as a persisted-unavailable
                // placeholder so they can see what was dropped.
                UnavailableReason::PersistedUnavailable
            };
            index.roots.push(RootEntry::Unavailable {
                configured_path: persisted_path,
                reason,
            });
        }

        // Assign each track to its most-specific configured root.
        for (index_pos, track_path) in track_paths.iter().enumerate() {
            let Some(path) = track_path else {
                index.unrooted_track_count += 1;
                continue;
            };
            let owner = match most_specific_root(&sorted_roots, path) {
                Some(root) => root.clone(),
                None => {
                    index.unrooted_track_count += 1;
                    continue;
                }
            };
            // Skip tracks whose owning root is not currently available.
            let owner_entry = index
                .roots
                .iter()
                .find(|entry| entry.configured_path() == owner.as_path());
            if !matches!(owner_entry, Some(RootEntry::Available(_))) {
                continue;
            }
            let relative = match path.strip_prefix(&owner) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => {
                    index.unrooted_track_count += 1;
                    continue;
                }
            };
            let parent = relative.parent().map(Path::to_path_buf).unwrap_or_default();
            let entry = index
                .root_subdirs
                .entry(owner.clone())
                .or_default()
                .entry(parent)
                .or_default();
            entry.direct += 1;
            // Walk ancestor directories so each one knows it has descendants.
            let mut current = relative.clone();
            while let Some(parent_dir) = current.parent() {
                if parent_dir.as_os_str().is_empty() {
                    break;
                }
                let parent_buf = parent_dir.to_path_buf();
                let parent_entry = index
                    .root_subdirs
                    .entry(owner.clone())
                    .or_default()
                    .entry(parent_buf)
                    .or_default();
                if !parent_entry.has_descendants {
                    parent_entry.has_descendants = true;
                }
                current = parent_dir.to_path_buf();
            }
            index
                .root_tracks
                .entry(owner)
                .or_default()
                .push(index_pos);
        }

        // Promote root-direct counts.
        for root_entry in &mut index.roots {
            if let RootEntry::Available(summary) = root_entry {
                let subdirs = index
                    .root_subdirs
                    .get(&summary.configured_path)
                    .cloned()
                    .unwrap_or_default();
                summary.direct_track_count = subdirs
                    .get(Path::new(""))
                    .map(|stats| stats.direct)
                    .unwrap_or(0);
                summary.child_dir_count = subdirs
                    .iter()
                    .filter(|(dir, _)| !dir.as_os_str().is_empty())
                    .count();
            }
        }

        index
    }

    /// Configured roots in display order (deepest first so the deepest
    /// configured ancestor wins disambiguation in the tree view).
    pub fn roots(&self) -> &[RootEntry] {
        &self.roots
    }

    /// Number of tracks whose `file_path` is not under any configured root.
    pub fn unrooted_track_count(&self) -> usize {
        self.unrooted_track_count
    }

    /// What the pane should show when there is nothing to browse.
    ///
    /// The pane consults the active source to decide whether to show
    /// `PathlessSource`; this helper reports the data-layer state.
    pub fn omission_notice(&self) -> Option<OmissionNotice> {
        if self.roots.is_empty() {
            return Some(OmissionNotice::NoRootsConfigured);
        }
        let available_roots = self.roots.iter().filter(|r| r.is_available()).count();
        if available_roots == 0 {
            // Configured but all unavailable; still show those rows rather
            // than an omission placeholder.
            return None;
        }
        let any_tracks_under_roots = self
            .roots
            .iter()
            .filter(|r| r.is_available())
            .any(|r| matches!(r, RootEntry::Available(s) if s.direct_track_count > 0));
        if !any_tracks_under_roots {
            Some(OmissionNotice::NoTracksUnderRoots)
        } else {
            None
        }
    }

    /// Immediate child directories of an expanded node, lazily read from
    /// disk.
    ///
    /// `parent_relative` is the root-relative directory whose children are
    /// wanted; pass `""` for the children of the root itself. The returned
    /// entries are sorted by name and deduplicated.
    ///
    /// `owner_root` is the configured root that owns this subtree; passing a
    /// path under a different root returns an empty list rather than leaking
    /// tracks across roots.
    pub fn child_dirs(
        &self,
        owner_root: &Path,
        parent_relative: &Path,
    ) -> std::io::Result<Vec<DirEntry>> {
        let owner = self
            .roots
            .iter()
            .find(|entry| entry.configured_path() == owner_root)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "owner root is not part of the configured library",
                )
            })?;
        if !owner.is_available() {
            return Ok(Vec::new());
        }
        if !is_contained_relative(parent_relative) {
            return Ok(Vec::new());
        }
        let absolute = owner_root.join(parent_relative);
        let metadata = match std::fs::metadata(&absolute) {
            Ok(meta) if meta.is_dir() => meta,
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "child_dirs target is not a directory",
                ));
            }
            Err(error) => return Err(error),
        };
        // Permitted: read-only. Skip symlinks to avoid escaping the root.
        let _ = metadata;
        let mut children: Vec<DirEntry> = Vec::new();
        for entry in std::fs::read_dir(&absolute)? {
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() {
                continue;
            }
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() || name.starts_with('.') {
                // Hidden directories are not part of the public folder
                // browser. They still appear in the filesystem tree so
                // advanced users can rename them, but the browser stays
                // consistent with file managers' default hidden-file policy.
                continue;
            }
            let child_relative = if parent_relative.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                parent_relative.join(&name)
            };
            // Re-check containment: symlink races are mitigated above, but
            // belt-and-suspenders: refuse to surface anything that escapes.
            if !is_contained_relative(&child_relative) {
                continue;
            }
            let stats = self
                .root_subdirs
                .get(owner_root)
                .and_then(|map| map.get(&child_relative))
                .cloned()
                .unwrap_or_default();
            children.push(DirEntry {
                name,
                root_relative: child_relative.to_string_lossy().replace('\\', "/"),
                direct_track_count: stats.direct,
                has_descendants: stats.has_descendants,
            });
        }
        children.sort_by_key(|c| c.name.to_lowercase());
        Ok(children)
    }

    /// Track indices in the original slice that live under
    /// `parent_relative` inside `owner_root`. Returned indices are in slice
    /// order so callers can sort them with a stable secondary key.
    pub fn dir_tracks(&self, owner_root: &Path, parent_relative: &Path) -> Vec<TrackRef> {
        let Some(track_indices) = self.root_tracks.get(owner_root) else {
            return Vec::new();
        };
        if !is_contained_relative(parent_relative) {
            return Vec::new();
        }
        let Some(owner) = self
            .roots
            .iter()
            .find(|entry| entry.configured_path() == owner_root)
        else {
            return Vec::new();
        };
        if !owner.is_available() {
            return Vec::new();
        }
        track_indices
            .iter()
            .copied()
            .filter(|idx| {
                self.track_paths
                    .get(*idx)
                    .and_then(|p| p.as_ref())
                    .and_then(|p| p.strip_prefix(owner_root).ok())
                    .and_then(|rel| rel.parent())
                    .map(|parent| parent == parent_relative)
                    .unwrap_or(false)
            })
            .map(|index| TrackRef {
                index,
                root_relative: parent_relative.to_path_buf(),
            })
            .collect()
    }

    /// Total track count beneath a node, including its descendants. Used by
    /// the row badge to give users a sense of branch size without expanding
    /// every directory.
    pub fn subtree_track_count(&self, owner_root: &Path, parent_relative: &Path) -> usize {
        if !is_contained_relative(parent_relative) {
            return 0;
        }
        self.root_tracks
            .get(owner_root)
            .map(|indices| {
                indices
                    .iter()
                    .filter(|idx| {
                        self.track_paths
                            .get(**idx)
                            .and_then(|p| p.as_ref())
                            .and_then(|p| p.strip_prefix(owner_root).ok())
                            .map(|rel| {
                                if parent_relative.as_os_str().is_empty() {
                                    true
                                } else {
                                    rel.starts_with(parent_relative)
                                }
                            })
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

fn classify_root(
    configured: &Path,
    persisted_by_path: &BTreeMap<&str, &library_root::Model>,
) -> RootEntry {
    let canonical = canonical_key(configured);
    let persisted = persisted_by_path.get(canonical.as_str()).copied();
    // Persisted unavailability wins over disk existence: the engine has
    // authoritative state, and a row marked unavailable means the root was
    // deliberately retired. A coincidental directory reappearing at the
    // path is not a re-trust signal.
    if let Some(model) = persisted {
        if !model.is_available {
            return RootEntry::Unavailable {
                configured_path: configured.to_path_buf(),
                reason: UnavailableReason::PersistedUnavailable,
            };
        }
        if !model.last_scan_complete {
            return RootEntry::Unavailable {
                configured_path: configured.to_path_buf(),
                reason: UnavailableReason::ScanIncomplete,
            };
        }
    }
    if !configured.exists() {
        return RootEntry::Unavailable {
            configured_path: configured.to_path_buf(),
            reason: UnavailableReason::Missing,
        };
    }
    RootEntry::Available(RootSummary {
        configured_path: configured.to_path_buf(),
        direct_track_count: 0,
        child_dir_count: 0,
    })
}

/// Return the deepest configured root that owns `path`. None when no
/// configured root contains the path.
fn most_specific_root<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
    roots
        .iter()
        .filter(|root| path.starts_with(root.as_path()))
        .max_by_key(|root| root.components().count())
}

/// Canonical key for a configured path. Used only for persisted-row lookup;
/// comparison is exact (no normalization) so two roots that differ only in
/// case remain distinct.
fn canonical_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Reject paths that try to escape the root via `..` or absolute prefixes.
fn is_contained_relative(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => return false,
            std::path::Component::Prefix(_) => return false,
            std::path::Component::RootDir => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track_paths(values: &[&str]) -> Vec<Option<PathBuf>> {
        values
            .iter()
            .map(|value| {
                if value.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            })
            .collect()
    }

    #[test]
    fn empty_configuration_reports_no_roots_notice() {
        let index = FolderIndex::build(
            Vec::<PathBuf>::new(),
            &[],
            track_paths(&["/music/a/track.flac"]),
        );
        assert!(index.roots().is_empty());
        assert_eq!(
            index.omission_notice(),
            Some(OmissionNotice::NoRootsConfigured)
        );
        assert_eq!(index.unrooted_track_count(), 1);
    }

    #[test]
    fn pathless_tracks_are_omitted_and_counted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let track_under_root = root.join("track.flac");
        std::fs::write(&track_under_root, b"").expect("write");

        let index = FolderIndex::build(
            vec![root.clone()],
            &[],
            track_paths(&[
                track_under_root.to_str().unwrap(),
                "",
            ]),
        );
        assert_eq!(index.roots().len(), 1);
        assert!(index.roots()[0].is_available());
        // Pathless track is counted as unrooted, not silently dropped.
        assert_eq!(index.unrooted_track_count(), 1);
    }

    #[test]
    fn tracks_under_no_configured_root_emit_no_tracks_under_roots_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let configured = dir.path().join("library");
        std::fs::create_dir_all(&configured).expect("mkdir");
        let stray = dir.path().join("stray/track.flac");
        std::fs::create_dir_all(stray.parent().unwrap()).expect("mkdir parent");
        std::fs::write(&stray, b"").expect("write");

        let index = FolderIndex::build(
            vec![configured],
            &[],
            track_paths(&[stray.to_str().unwrap()]),
        );
        assert!(index
            .roots()
            .iter()
            .any(|r| matches!(r, RootEntry::Available(_))));
        assert_eq!(
            index.omission_notice(),
            Some(OmissionNotice::NoTracksUnderRoots)
        );
    }

    #[test]
    fn overlapping_roots_assign_tracks_to_deepest_root_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().join("music");
        let nested = parent.join("classical");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let track = nested.join("track.flac");
        std::fs::write(&track, b"").expect("write");

        let index = FolderIndex::build(
            vec![parent.clone(), nested.clone()],
            &[],
            track_paths(&[track.to_str().unwrap()]),
        );
        let summaries: Vec<&RootSummary> = index
            .roots()
            .iter()
            .filter_map(|r| match r {
                RootEntry::Available(summary) => Some(summary),
                _ => None,
            })
            .collect();
        assert_eq!(summaries.len(), 2);
        let nested_summary = summaries
            .iter()
            .find(|s| s.configured_path == nested)
            .expect("nested root present");
        let parent_summary = summaries
            .iter()
            .find(|s| s.configured_path == parent)
            .expect("parent root present");
        assert_eq!(nested_summary.direct_track_count, 1);
        assert_eq!(parent_summary.direct_track_count, 0);
    }

    #[test]
    fn missing_root_is_marked_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let phantom = dir.path().join("does-not-exist");
        let index = FolderIndex::build(
            vec![phantom.clone()],
            &[],
            track_paths(&[phantom.join("track.flac").to_str().unwrap()]),
        );
        assert_eq!(
            index.roots(),
            &[RootEntry::Unavailable {
                configured_path: phantom,
                reason: UnavailableReason::Missing,
            }]
        );
    }

    #[test]
    fn persisted_unavailable_root_is_marked_persisted_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let persisted = library_root::Model {
            path: root.to_string_lossy().into_owned(),
            device_id: None,
            identity_confirmed: true,
            is_available: false,
            last_scan_complete: true,
            last_checked_at: "2026-07-22T00:00:00Z".to_string(),
        };
        let index = FolderIndex::build(
            vec![root.clone()],
            std::slice::from_ref(&persisted),
            track_paths(&[]),
        );
        assert_eq!(
            index.roots(),
            &[RootEntry::Unavailable {
                configured_path: root,
                reason: UnavailableReason::PersistedUnavailable,
            }]
        );
    }

    #[test]
    fn orphaned_persisted_root_surfaces_as_unavailable_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let configured = dir.path().to_path_buf();
        // tempdir already created the directory; we only need an orphaned
        // persisted root under a subpath that we deliberately do not
        // create on disk.
        let orphaned = dir.path().join("removed-from-prefs");
        let persisted = library_root::Model {
            path: orphaned.to_string_lossy().into_owned(),
            device_id: None,
            identity_confirmed: true,
            is_available: false,
            last_scan_complete: true,
            last_checked_at: "2026-07-22T00:00:00Z".to_string(),
        };
        let index = FolderIndex::build(
            vec![configured.clone()],
            std::slice::from_ref(&persisted),
            track_paths(&[]),
        );
        // The configured root appears as Available.
        assert!(matches!(
            &index.roots()[0],
            RootEntry::Available(summary) if summary.configured_path == configured
        ));
        // The orphaned persisted root appears as a placeholder.
        assert!(index
            .roots()
            .iter()
            .any(|r| matches!(r, RootEntry::Unavailable { configured_path, reason: UnavailableReason::PersistedUnavailable } if *configured_path == orphaned)));
    }

    #[test]
    fn child_dirs_returns_immediate_subdirectories_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("alpha")).expect("mkdir alpha");
        std::fs::create_dir(root.join("beta")).expect("mkdir beta");
        std::fs::create_dir(root.join("alpha/nested")).expect("mkdir nested");

        let alpha = root.join("alpha/track.flac");
        let nested = root.join("alpha/nested/track.flac");
        std::fs::write(&alpha, b"").expect("write alpha");
        std::fs::write(&nested, b"").expect("write nested");

        let index = FolderIndex::build(
            vec![root.clone()],
            &[],
            track_paths(&[
                alpha.to_str().unwrap(),
                nested.to_str().unwrap(),
            ]),
        );
        let children = index
            .child_dirs(&root, Path::new(""))
            .expect("child_dirs");
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        let alpha_entry = children.iter().find(|c| c.name == "alpha").expect("alpha");
        assert_eq!(alpha_entry.direct_track_count, 1);
        assert!(alpha_entry.has_descendants);

        let alpha_children = index
            .child_dirs(&root, Path::new("alpha"))
            .expect("child_dirs(alpha)");
        assert_eq!(
            alpha_children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["nested"]
        );
    }

    #[test]
    fn child_dirs_rejects_path_escape_attempts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let index = FolderIndex::build(vec![root.clone()], &[], track_paths(&[]));
        assert!(index.child_dirs(&root, Path::new("../etc")).unwrap().is_empty());
        assert!(index
            .child_dirs(&root, Path::new("/etc"))
            .unwrap()
            .is_empty());
        let absolute = root.join("..").join("etc");
        assert!(index.child_dirs(&root, &absolute).unwrap().is_empty());
    }

    #[test]
    fn dir_tracks_returns_only_tracks_directly_under_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("album")).expect("mkdir album");
        let track_in_root = root.join("track.flac");
        let track_in_album = root.join("album/track.flac");
        std::fs::write(&track_in_root, b"").expect("write root");
        std::fs::write(&track_in_album, b"").expect("write album");

        let index = FolderIndex::build(
            vec![root.clone()],
            &[],
            track_paths(&[
                track_in_root.to_str().unwrap(),
                track_in_album.to_str().unwrap(),
            ]),
        );
        let root_tracks = index.dir_tracks(&root, Path::new(""));
        assert_eq!(root_tracks.len(), 1);
        assert_eq!(root_tracks[0].index, 0);

        let album_tracks = index.dir_tracks(&root, Path::new("album"));
        assert_eq!(album_tracks.len(), 1);
        assert_eq!(album_tracks[0].index, 1);

        assert!(index.dir_tracks(&root, Path::new("missing")).is_empty());
    }

    #[test]
    fn dir_tracks_for_a_non_root_returns_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let configured = dir.path().to_path_buf();
        let other = dir.path().join("other");
        std::fs::create_dir(&other).expect("mkdir other");
        let track = other.join("track.flac");
        std::fs::write(&track, b"").expect("write");

        let index = FolderIndex::build(
            vec![configured.clone()],
            &[],
            track_paths(&[track.to_str().unwrap()]),
        );
        assert!(index.dir_tracks(&other, Path::new("")).is_empty());
    }

    #[test]
    fn subtree_track_count_walks_descendants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("album")).expect("mkdir album");
        let top = root.join("track.flac");
        let nested = root.join("album/track.flac");
        std::fs::write(&top, b"").expect("write");
        std::fs::write(&nested, b"").expect("write");

        let index = FolderIndex::build(
            vec![root.clone()],
            &[],
            track_paths(&[top.to_str().unwrap(), nested.to_str().unwrap()]),
        );
        assert_eq!(index.subtree_track_count(&root, Path::new("")), 2);
        assert_eq!(index.subtree_track_count(&root, Path::new("album")), 1);
    }

    #[test]
    fn child_dirs_skips_hidden_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("visible")).expect("mkdir visible");
        std::fs::create_dir(root.join(".hidden")).expect("mkdir hidden");
        let index = FolderIndex::build(vec![root.clone()], &[], track_paths(&[]));
        let names: Vec<String> = index
            .child_dirs(&root, Path::new(""))
            .expect("child_dirs")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["visible".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn child_dirs_skips_symlinked_subdirectories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir(root.join("real")).expect("mkdir real");
        let link_result = std::os::unix::fs::symlink(root.join("real"), root.join("link"));
        if link_result.is_err() {
            return;
        }
        let index = FolderIndex::build(vec![root.clone()], &[], track_paths(&[]));
        let names: Vec<String> = index
            .child_dirs(&root, Path::new(""))
            .expect("child_dirs")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["real".to_string()]);
    }
}