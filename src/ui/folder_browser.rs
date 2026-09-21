//! Root-relative folder browsing over the local library catalog (#14).
//!
//! Pure model — no GTK types — so every rule here is deterministic and
//! unit-tested:
//!
//! * **Root-relative paths.** A track is placed under the configured library
//!   root that contains it; the browser navigates the remaining relative
//!   path. Nothing outside a configured root is shown.
//! * **Portable navigation, native comparison.** Navigation `dir` strings
//!   use a deliberately portable representation: `/`-separated, root-relative
//!   paths. Every comparison against stored track paths runs over native
//!   [`Path`] components, so Windows relatives (`\`-separated) derive the
//!   same folder tree as Unix ones instead of being split on `/` only.
//! * **Multi-root disambiguation.** Every root is its own namespace: the same
//!   relative folder under two roots is two distinct destinations. Root rows
//!   with identical display names get deterministic, distinct labels.
//! * **Lazy navigation.** Nothing is walked up front. Descending into a
//!   directory derives only that level's subdirectories (from the indexed
//!   track paths) at the moment it is asked for.
//! * **Unavailable / renamed roots.** A root whose path is missing or unreadable
//!   is listed as unavailable; a root whose recorded identity no longer matches
//!   its path is listed as renamed. Both remain visible (with the reason) but
//!   refuse navigation instead of silently disappearing or appearing empty.
//! * **Explicit omission policy.** Tracks from pathless sources (radio,
//!   remotes without filesystem semantics) and local paths outside every
//!   configured root are NOT shown; every omission is reported back so the
//!   policy is a decision, never an accident.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Availability of one configured library root as folder browsing sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootAvailability {
    /// The path exists and is a readable directory.
    Available,
    /// The configured path does not exist or is not a directory.
    Unavailable { reason: String },
    /// The path exists, but the recorded root identity no longer matches it —
    /// the directory was likely renamed or replaced after the library scan.
    Renamed { previous_path: String },
}

/// One configured library root as folder browsing sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowsableRoot {
    /// Stable identity: the configured path text itself.
    pub root_id: String,
    /// Display name: the root path's final component.
    pub display_name: String,
    /// Absolute configured root path.
    pub root_path: PathBuf,
    /// Availability observed at construction time.
    pub availability: RootAvailability,
}

impl BrowsableRoot {
    /// Inspect one configured root path.
    ///
    /// `recorded_path` is the path the library scan last confirmed for this
    /// root's identity, when one exists: if the configured path now names a
    /// different directory than the recorded one, the root is reported as
    /// [`RootAvailability::Renamed`] rather than silently browsed.
    pub fn from_configured(path_text: &str, recorded_path: Option<&str>) -> Self {
        let root_path = PathBuf::from(path_text);
        let display_name = root_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_text.to_string());
        let availability = match std::fs::metadata(&root_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                RootAvailability::Unavailable {
                    reason: "folder is missing".to_string(),
                }
            }
            Err(error) => RootAvailability::Unavailable {
                reason: format!("folder is not readable: {error}"),
            },
            Ok(metadata) if !metadata.is_dir() => RootAvailability::Unavailable {
                reason: "configured path is not a folder".to_string(),
            },
            Ok(_) => {
                // The path exists. A recorded identity that points somewhere
                // else means this path is (likely) a renamed or replaced
                // library folder.
                if let Some(recorded) = recorded_path {
                    if recorded != path_text && Path::new(recorded).exists() {
                        RootAvailability::Renamed {
                            previous_path: recorded.to_string(),
                        }
                    } else {
                        RootAvailability::Available
                    }
                } else {
                    RootAvailability::Available
                }
            }
        };
        Self {
            root_id: path_text.to_string(),
            display_name,
            root_path,
            availability,
        }
    }

    /// Whether navigation into this root may proceed.
    pub fn browsable(&self) -> bool {
        self.availability == RootAvailability::Available
    }

    /// Suffix describing a non-available root, for display next to its name.
    pub fn availability_suffix(&self) -> Option<String> {
        match &self.availability {
            RootAvailability::Available => None,
            RootAvailability::Unavailable { reason } => Some(format!(" (unavailable: {reason})")),
            RootAvailability::Renamed { previous_path } => {
                Some(format!(" (renamed from {previous_path})"))
            }
        }
    }
}

/// One track offered to folder placement. `path` is `None` for tracks from
/// sources without filesystem semantics (radio, remotes without paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPathInput {
    pub source_label: String,
    pub path: Option<PathBuf>,
}

/// One reported omission — the explicit side of the omission policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Omission {
    pub reason: String,
    /// How many tracks this omission covers.
    pub count: usize,
}

/// Everything the placement step decided not to show, and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowsingReport {
    pub omissions: Vec<Omission>,
}

impl BrowsingReport {
    fn record(&mut self, reason: String, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(existing) = self.omissions.iter_mut().find(|o| o.reason == reason) {
            existing.count += count;
        } else {
            self.omissions.push(Omission { reason, count });
        }
    }
}

/// A track successfully placed under a root, keyed by the root's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedTrack {
    pub root_id: String,
    /// Path relative to the root (never empty, never absolute).
    pub relative: PathBuf,
}

/// Place tracks under their containing roots.
///
/// Tracks without a path are omitted with a per-source reason; local paths
/// that no configured root contains are omitted as outside the library.
/// A track whose path sits under multiple roots (nested roots) is placed
/// under the most specific (longest) root.
pub fn place_tracks(
    roots: &[BrowsableRoot],
    tracks: &[TrackPathInput],
) -> (Vec<PlacedTrack>, BrowsingReport) {
    let mut report = BrowsingReport::default();
    let mut placed = Vec::new();
    for track in tracks {
        let Some(path) = track.path.as_deref() else {
            report.record(
                format!("source “{}” has no filesystem paths", track.source_label),
                1,
            );
            continue;
        };
        let best = roots
            .iter()
            .filter(|root| root.browsable() && path.starts_with(&root.root_path))
            .max_by_key(|root| root.root_path.as_os_str().len());
        let Some(root) = best else {
            report.record(
                format!("path is outside every configured root ({})", path.display()),
                1,
            );
            continue;
        };
        let Ok(relative) = path.strip_prefix(&root.root_path) else {
            report.record(
                format!("path is outside every configured root ({})", path.display()),
                1,
            );
            continue;
        };
        if relative.as_os_str().is_empty() {
            // The root itself is not a track.
            continue;
        }
        placed.push(PlacedTrack {
            root_id: root.root_id.clone(),
            relative: relative.to_path_buf(),
        });
    }
    (placed, report)
}

/// One subdirectory entry at the currently browsed level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChild {
    /// Directory name (final component only).
    pub name: String,
    /// Root-relative directory path of this child.
    pub dir: String,
    /// Number of tracks anywhere beneath this subtree (for the count label).
    pub track_count: usize,
}

/// Why navigation into a root cannot proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootBrowseError {
    Unavailable { reason: String },
    Renamed { previous_path: String },
    UnknownRoot,
}

/// The lazy folder tree over one library's placed tracks.
#[derive(Debug, Clone, Default)]
pub struct FolderBrowser {
    roots: Vec<BrowsableRoot>,
    /// Root id → every placed relative path. No tree is prebuilt; each level
    /// is derived on demand from this flat set.
    by_root: BTreeMap<String, Vec<PathBuf>>,
}

impl FolderBrowser {
    pub fn new(roots: Vec<BrowsableRoot>, placed: Vec<PlacedTrack>) -> Self {
        let mut by_root: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for track in placed {
            by_root
                .entry(track.root_id)
                .or_default()
                .push(track.relative);
        }
        for paths in by_root.values_mut() {
            paths.sort();
        }
        Self { roots, by_root }
    }

    /// The configured roots, in stable order.
    pub fn roots(&self) -> &[BrowsableRoot] {
        &self.roots
    }

    /// Root display labels with deterministic disambiguation: when two roots
    /// share a final component, each is suffixed with a numbered index so
    /// the two rows can never look identical.
    pub fn disambiguated_root_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = Vec::with_capacity(self.roots.len());
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for root in &self.roots {
            let duplicates = seen.entry(root.display_name.clone()).or_insert(0);
            *duplicates += 1;
            if roots_with_name(&self.roots, &root.display_name) > 1 {
                labels.push(format!("{} · {}", root.display_name, *duplicates));
            } else {
                labels.push(root.display_name.clone());
            }
        }
        labels
    }

    /// One level's subdirectories under `dir` (empty string = the root
    /// itself). `dir` uses the portable representation: a `/`-separated,
    /// root-relative path as emitted by [`FolderChild::dir`]. Computed on
    /// demand — this is the lazy navigation step. Comparison runs over
    /// native [`Path`] components, so stored relatives with Windows
    /// separators derive the same tree as Unix ones. Directories are
    /// returned sorted case-insensitively by name; the count covers the
    /// whole subtree.
    pub fn children(&self, root_id: &str, dir: &str) -> Result<Vec<FolderChild>, RootBrowseError> {
        let root = self
            .roots
            .iter()
            .find(|root| root.root_id == root_id)
            .ok_or(RootBrowseError::UnknownRoot)?;
        match &root.availability {
            RootAvailability::Unavailable { reason } => {
                return Err(RootBrowseError::Unavailable {
                    reason: reason.clone(),
                });
            }
            RootAvailability::Renamed { previous_path } => {
                return Err(RootBrowseError::Renamed {
                    previous_path: previous_path.clone(),
                });
            }
            RootAvailability::Available => {}
        }
        let normalized = normalize_dir(dir);
        let dir_path = portable_dir_path(&normalized);
        let prefix = if normalized.is_empty() {
            String::new()
        } else {
            format!("{normalized}/")
        };
        let paths = self.by_root.get(root_id).map(Vec::as_slice).unwrap_or(&[]);
        // One pass over this root's paths derives the level: compare with
        // native path components. The stored relatives carry the native
        // separators of the platform that indexed them (`\` on Windows), so
        // a text split on `/` would lose every level below the root there.
        // Collect the first component below the browsed directory, plus
        // subtree track counts.
        let mut names: BTreeMap<String, usize> = BTreeMap::new();
        for relative in paths {
            let Ok(rest) = relative.strip_prefix(&dir_path) else {
                continue;
            };
            let mut components = rest.components();
            let Some(name) = components.next() else {
                continue;
            };
            // Only directories below this level are children. A track
            // file sitting directly in the browsed directory belongs to
            // the tracklist filter, not to the folder pane.
            if components.next().is_none() {
                continue;
            }
            *names
                .entry(name.as_os_str().to_string_lossy().into_owned())
                .or_insert(0) += 1;
        }
        let mut children: Vec<FolderChild> = names
            .into_iter()
            .map(|(name, count)| {
                let dir = if normalized.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}{name}")
                };
                FolderChild {
                    name,
                    dir,
                    track_count: count,
                }
            })
            .collect();
        children.sort_by_key(|child| child.name.to_lowercase());
        Ok(children)
    }
}

fn roots_with_name(roots: &[BrowsableRoot], name: &str) -> usize {
    roots
        .iter()
        .filter(|root| root.display_name == name)
        .count()
}

/// Normalize a user/model directory string to the portable representation:
/// a `/`-separated, root-relative path with no empty or `.` components and
/// no escape — `..` pops one level and clamps at the root, so the result
/// can never name anything above the root it is relative to. An empty input
/// normalizes to the root itself (`""`).
///
/// The portable separator is `/` by definition; a literal `\` is an
/// ordinary name character here (on Windows names can contain neither
/// separator, so the round-trip through native [`Path`] components is
/// exact). A component that smuggles the platform's native separator
/// ([`is_atomic_component`]) can never match an indexed path and is dropped
/// by the comparison helpers.
fn normalize_dir(dir: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in dir.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Whether `component` can be a single path component on this platform.
/// Portable parsing already excluded `/`; on Windows the native separator
/// `\` must equally never appear inside one, or a crafted dir string could
/// smuggle parent traversal (`..\..`) past the no-escape clamp.
fn is_atomic_component(component: &str) -> bool {
    !component.contains(std::path::MAIN_SEPARATOR)
}

/// Convert a normalized portable `dir` into a native relative path whose
/// components can be compared with the stored relatives. Components that
/// are not atomic on this platform are dropped (they can never match).
fn portable_dir_path(normalized: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for component in normalized.split('/').filter(|c| is_atomic_component(c)) {
        path.push(component);
    }
    path
}

/// Join an absolute configured root path with a portable root-relative
/// `dir` into a native path for the running platform (no trailing
/// separator). `dir` is normalized with the same rules navigation uses —
/// empty and `.` components are dropped, `..` pops a level and clamps at
/// the root, and components carrying the native separator are refused —
/// so the result can never name anything outside `root`, and an empty
/// `dir` yields `root` itself. Folder filtering builds its track-filter
/// prefix from this, keeping that boundary on native separators too.
pub fn join_native(root: &str, dir: &str) -> PathBuf {
    let mut path = PathBuf::from(root);
    for component in normalize_dir(dir)
        .split('/')
        .filter(|c| is_atomic_component(c))
    {
        path.push(component);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available_root(path_text: &str) -> BrowsableRoot {
        BrowsableRoot {
            root_id: path_text.to_string(),
            display_name: Path::new(path_text)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_text.to_string()),
            root_path: PathBuf::from(path_text),
            availability: RootAvailability::Available,
        }
    }

    fn input(label: &str, path: &str) -> TrackPathInput {
        TrackPathInput {
            source_label: label.to_string(),
            path: Some(PathBuf::from(path)),
        }
    }

    /// A track input whose path is built by joining native components onto
    /// the root, so the stored relatives carry the platform separator
    /// (`\` on Windows, `/` on Unix) exactly as the local scanner produces
    /// them. This is what makes these tests native-component fixtures on
    /// every platform instead of Unix-literal strings.
    fn input_at(root: &BrowsableRoot, components: &[&str]) -> TrackPathInput {
        let mut path = root.root_path.clone();
        for component in components {
            path.push(component);
        }
        TrackPathInput {
            source_label: "local".to_string(),
            path: Some(path),
        }
    }

    fn child(name: &str, dir: &str, track_count: usize) -> FolderChild {
        FolderChild {
            name: name.to_string(),
            dir: dir.to_string(),
            track_count,
        }
    }

    #[test]
    fn place_tracks_namespaces_by_root_and_reports_omissions() {
        let roots = vec![available_root("/music/a"), available_root("/music/b")];
        let tracks = vec![
            input("local", "/music/a/Artist/Album/01.flac"),
            input("local", "/music/b/Artist/Album/01.flac"),
            input("local", "/elsewhere/song.flac"),
            TrackPathInput {
                source_label: "radio".to_string(),
                path: None,
            },
            TrackPathInput {
                source_label: "radio".to_string(),
                path: None,
            },
        ];
        let (placed, report) = place_tracks(&roots, &tracks);
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].root_id, "/music/a");
        assert_eq!(placed[0].relative, PathBuf::from("Artist/Album/01.flac"));
        assert_eq!(placed[1].root_id, "/music/b");
        // Each omission class is reported once with its count.
        assert_eq!(report.omissions.len(), 2);
        assert!(report.omissions.iter().any(|o| {
            o.reason.contains("radio") && o.reason.contains("no filesystem paths") && o.count == 2
        }));
        assert!(report
            .omissions
            .iter()
            .any(|o| o.reason.contains("outside every configured root") && o.count == 1));
    }

    #[test]
    fn nested_roots_place_under_the_most_specific_root() {
        let roots = vec![available_root("/music"), available_root("/music/nested")];
        let tracks = vec![
            input("local", "/music/top.flac"),
            input("local", "/music/nested/deep.flac"),
        ];
        let (placed, _) = place_tracks(&roots, &tracks);
        assert_eq!(placed[0].relative, PathBuf::from("top.flac"));
        assert_eq!(placed[1].root_id, "/music/nested");
        assert_eq!(placed[1].relative, PathBuf::from("deep.flac"));
    }

    #[test]
    fn children_are_lazy_sorted_and_counted_per_subtree() {
        let roots = vec![available_root("/music")];
        let tracks = vec![
            input("local", "/music/rock/a/1.flac"),
            input("local", "/music/rock/a/2.flac"),
            input("local", "/music/rock/b/3.flac"),
            input("local", "/music/jazz/4.flac"),
            input("local", "/music/top.flac"),
        ];
        let (placed, report) = place_tracks(&roots, &tracks);
        assert!(report.omissions.is_empty());
        let browser = FolderBrowser::new(roots, placed);

        // The root level: only folders with tracks, sorted case-insensitively.
        let children = browser.children("/music", "").expect("root level browses");
        assert_eq!(
            children,
            vec![
                FolderChild {
                    name: "jazz".to_string(),
                    dir: "jazz".to_string(),
                    track_count: 1,
                },
                FolderChild {
                    name: "rock".to_string(),
                    dir: "rock".to_string(),
                    track_count: 3,
                },
            ]
        );

        // Descending is on demand: only the asked level is derived.
        let level = browser
            .children("/music", "rock")
            .expect("nested level browses");
        assert_eq!(
            level,
            vec![
                FolderChild {
                    name: "a".to_string(),
                    dir: "rock/a".to_string(),
                    track_count: 2,
                },
                FolderChild {
                    name: "b".to_string(),
                    dir: "rock/b".to_string(),
                    track_count: 1,
                },
            ]
        );
        assert!(browser.children("/music", "rock/a").unwrap().is_empty());
    }

    #[test]
    fn children_derive_from_native_relative_components() {
        let roots = vec![available_root("/music")];
        let tracks = vec![
            input_at(&roots[0], &["Artist", "Album", "01.flac"]),
            input_at(&roots[0], &["Artist", "Album", "02.flac"]),
            input_at(&roots[0], &["Artist", "Singles", "03.flac"]),
            input_at(&roots[0], &["top.flac"]),
        ];
        let (placed, report) = place_tracks(&roots, &tracks);
        assert!(report.omissions.is_empty());
        // The stored relatives are native on every platform: on Windows
        // they carry `\`, and the pre-fix derivation (split on `/` only)
        // lost every level below the root there, emitting no folder
        // children at all.
        assert_eq!(placed[0].relative.components().count(), 3);
        let browser = FolderBrowser::new(roots, placed);

        // Multiple levels: the root level and each descent derive from
        // native components, while the emitted dirs stay portable.
        let root_level = browser.children("/music", "").expect("root level browses");
        assert_eq!(root_level, vec![child("Artist", "Artist", 3)]);

        let artist = browser
            .children("/music", "Artist")
            .expect("artist browses");
        assert_eq!(
            artist,
            vec![
                child("Album", "Artist/Album", 2),
                child("Singles", "Artist/Singles", 1),
            ]
        );

        // Files directly in the browsed directory are not folder children.
        assert!(browser
            .children("/music", "Artist/Album")
            .expect("album browses")
            .is_empty());
    }

    #[test]
    fn siblings_with_common_prefixes_stay_distinct() {
        let roots = vec![available_root("/music")];
        let tracks = vec![
            input_at(&roots[0], &["Artist", "Greatest", "1.flac"]),
            input_at(&roots[0], &["Artist Deluxe", "Greatest", "2.flac"]),
            input_at(&roots[0], &["Artist", "Rare", "3.flac"]),
        ];
        let (placed, _) = place_tracks(&roots, &tracks);
        let browser = FolderBrowser::new(roots, placed);

        // Component matching, never text-prefix matching: "Artist" and
        // "Artist Deluxe" are two separate children.
        let root_level = browser.children("/music", "").expect("root level browses");
        assert_eq!(
            root_level,
            vec![
                child("Artist", "Artist", 2),
                child("Artist Deluxe", "Artist Deluxe", 1),
            ]
        );

        // Descending into one must not absorb the sibling's subtree.
        assert_eq!(
            browser
                .children("/music", "Artist")
                .expect("Artist browses"),
            vec![
                child("Greatest", "Artist/Greatest", 1),
                child("Rare", "Artist/Rare", 1),
            ]
        );
        assert_eq!(
            browser
                .children("/music", "Artist Deluxe")
                .expect("Artist Deluxe browses"),
            vec![child("Greatest", "Artist Deluxe/Greatest", 1)]
        );
    }

    #[test]
    fn names_with_spaces_and_unicode_round_trip() {
        let roots = vec![available_root("/music")];
        let tracks = vec![
            input_at(&roots[0], &["Müsing Ensemble", "Live Été", "1.flac"]),
            input_at(&roots[0], &["Björn & Cie", "Studio", "2.flac"]),
        ];
        let (placed, _) = place_tracks(&roots, &tracks);
        let browser = FolderBrowser::new(roots, placed);

        let root_level = browser.children("/music", "").expect("root level browses");
        assert_eq!(
            root_level,
            vec![
                child("Björn & Cie", "Björn & Cie", 1),
                child("Müsing Ensemble", "Müsing Ensemble", 1),
            ]
        );

        // Spaces and non-ASCII survive every hop: descend by the portable
        // dir and the next level derives intact.
        let ensemble = browser
            .children("/music", "Müsing Ensemble")
            .expect("ensemble browses");
        assert_eq!(
            ensemble,
            vec![child("Live Été", "Müsing Ensemble/Live Été", 1)]
        );
    }

    #[test]
    fn dir_navigation_cannot_escape_the_root() {
        let roots = vec![available_root("/music")];
        let tracks = vec![
            input_at(&roots[0], &["rock", "a", "1.flac"]),
            input_at(&roots[0], &["jazz", "2.flac"]),
        ];
        let (placed, _) = place_tracks(&roots, &tracks);
        let browser = FolderBrowser::new(roots, placed);

        // The browsed set only ever contains indexed paths beneath the
        // root, and `..` clamps at the root: a crafted dir can surface
        // nothing outside it.
        assert_eq!(
            browser
                .children("/music", "../../outside")
                .expect("clamped dir browses"),
            Vec::new()
        );
        assert_eq!(
            browser
                .children("/music", "rock/../../jazz")
                .expect("clamped dir browses"),
            Vec::new()
        );
        // Sanity: the real children remain reachable.
        assert_eq!(
            browser.children("/music", "rock").expect("rock browses"),
            vec![child("a", "rock/a", 1)]
        );
    }

    #[test]
    fn join_native_joins_portable_components_and_clamps_escapes() {
        assert_eq!(join_native("/music", ""), PathBuf::from("/music"));
        assert_eq!(
            join_native("/music", "a/b"),
            PathBuf::from("/music").join("a").join("b")
        );
        assert_eq!(
            join_native("/music", "/a//b/."),
            PathBuf::from("/music").join("a").join("b")
        );
        // `..` clamps at the root; it can never climb above it.
        assert_eq!(join_native("/music", ".."), PathBuf::from("/music"));
        assert_eq!(
            join_native("/music", "../../etc"),
            PathBuf::from("/music").join("etc")
        );
        assert_eq!(
            join_native("/music", "a/../b"),
            PathBuf::from("/music").join("b")
        );
    }

    #[test]
    fn same_relative_folder_under_two_roots_is_two_destinations() {
        let roots = vec![available_root("/music/a"), available_root("/music/b")];
        let tracks = vec![
            input("local", "/music/a/Mix/1.flac"),
            input("local", "/music/b/Mix/2.flac"),
        ];
        let (placed, _) = place_tracks(&roots, &tracks);
        let browser = FolderBrowser::new(roots, placed);
        let under_a = browser.children("/music/a", "Mix").expect("root a browses");
        let under_b = browser.children("/music/b", "Mix").expect("root b browses");
        // Same name, different namespaces: a's folder holds track 1 only.
        assert!(under_a.is_empty());
        assert!(under_b.is_empty());
        assert_eq!(browser.disambiguated_root_labels(), vec!["a", "b"]);
    }

    #[test]
    fn duplicate_root_display_names_are_disambiguated_deterministically() {
        let roots = vec![available_root("/vault/music"), available_root("/usb/music")];
        let browser = FolderBrowser::new(roots, Vec::new());
        assert_eq!(
            browser.disambiguated_root_labels(),
            vec!["music · 1", "music · 2"]
        );
    }

    #[test]
    fn unavailable_and_renamed_roots_refuse_navigation_but_stay_listed() {
        let mut roots = vec![available_root("/music")];
        roots.push(BrowsableRoot {
            root_id: "/mnt/gone".to_string(),
            display_name: "gone".to_string(),
            root_path: PathBuf::from("/mnt/gone"),
            availability: RootAvailability::Unavailable {
                reason: "folder is missing".to_string(),
            },
        });
        roots.push(BrowsableRoot {
            root_id: "/mnt/moved-old".to_string(),
            display_name: "moved".to_string(),
            root_path: PathBuf::from("/mnt/moved-old"),
            availability: RootAvailability::Renamed {
                previous_path: "/mnt/moved-old".to_string(),
            },
        });
        let tracks = vec![input("local", "/music/x/1.flac")];
        let (placed, _) = place_tracks(&roots, &tracks);
        let browser = FolderBrowser::new(roots, placed.clone());

        // All three roots stay listed.
        assert_eq!(browser.roots().len(), 3);
        assert_eq!(
            browser.roots()[1].availability_suffix().as_deref(),
            Some(" (unavailable: folder is missing)")
        );

        // Navigation into them is refused with the recorded reason, even
        // though placement would have put tracks under a matching prefix.
        assert_eq!(
            browser.children("/mnt/gone", ""),
            Err(RootBrowseError::Unavailable {
                reason: "folder is missing".to_string(),
            })
        );
        assert_eq!(
            browser.children("/mnt/moved-old", ""),
            Err(RootBrowseError::Renamed {
                previous_path: "/mnt/moved-old".to_string(),
            })
        );
        assert_eq!(
            browser.children("/does-not-exist", ""),
            Err(RootBrowseError::UnknownRoot)
        );

        // Unavailable roots never capture placement.
        assert_eq!(placed.clone().len(), 1);
        assert_eq!(placed[0].root_id, "/music");
    }

    #[test]
    fn normalize_dir_is_idempotent_and_rejects_nothing() {
        assert_eq!(normalize_dir(""), "");
        assert_eq!(normalize_dir("a/b"), "a/b");
        assert_eq!(normalize_dir("/a/b/"), "a/b");
        assert_eq!(normalize_dir("a//b"), "a/b");
        assert_eq!(normalize_dir("a/./b"), "a/b");
        assert_eq!(normalize_dir("a/../b"), "b");
    }
}

/// Native Windows fixtures (#252). Drive roots, backslash separators,
/// spaces, Unicode, and prefix-sibling names are only expressible on a
/// Windows target, so these run in the Windows CI leg
/// (`cargo test --release --target <windows-target>`).
#[cfg(windows)]
mod windows_fixtures {
    use super::*;

    fn windows_root(path_text: &str) -> BrowsableRoot {
        BrowsableRoot {
            root_id: path_text.to_string(),
            display_name: Path::new(path_text)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_text.to_string()),
            root_path: PathBuf::from(path_text),
            availability: RootAvailability::Available,
        }
    }

    fn input_at(root: &BrowsableRoot, components: &[&str]) -> TrackPathInput {
        let mut path = root.root_path.clone();
        for component in components {
            path.push(component);
        }
        TrackPathInput {
            source_label: "local".to_string(),
            path: Some(path),
        }
    }

    fn child(name: &str, dir: &str, track_count: usize) -> FolderChild {
        FolderChild {
            name: name.to_string(),
            dir: dir.to_string(),
            track_count,
        }
    }

    /// URI filter prefix exactly as the window's filter builds it:
    /// canonical file URI with a trailing separator so `/a/b` cannot
    /// match `/a/bc`.
    fn uri_prefix(path: &Path) -> String {
        let mut text = url::Url::from_file_path(path)
            .expect("absolute path converts to a file URI")
            .to_string();
        if !text.ends_with('/') {
            text.push('/');
        }
        text
    }

    #[test]
    fn drive_root_levels_derive_from_backslash_relatives() {
        let roots = vec![windows_root("C:\\Music Library")];
        let tracks = vec![
            input_at(&roots[0], &["Artist", "Album", "01.flac"]),
            input_at(&roots[0], &["Artist", "Album", "02.flac"]),
            input_at(&roots[0], &["Artist", "Singles", "03.flac"]),
            input_at(&roots[0], &["top.flac"]),
        ];
        let (placed, report) = place_tracks(&roots, &tracks);
        assert!(report.omissions.is_empty());
        // The defect this fixture pins: placement is native (strip_prefix),
        // but the stored relatives carry `\`, and the pre-fix derivation
        // split their text on `/` only — no directory child was ever
        // emitted on Windows.
        let browser = FolderBrowser::new(roots, placed);

        let root_level = browser
            .children("C:\\Music Library", "")
            .expect("drive root browses");
        assert_eq!(root_level, vec![child("Artist", "Artist", 3)]);

        let artist = browser
            .children("C:\\Music Library", "Artist")
            .expect("artist level browses");
        assert_eq!(
            artist,
            vec![
                child("Album", "Artist/Album", 2),
                child("Singles", "Artist/Singles", 1),
            ]
        );

        assert!(browser
            .children("C:\\Music Library", "Artist/Album")
            .expect("album level browses")
            .is_empty());
    }

    #[test]
    fn drive_root_names_with_spaces_unicode_and_prefix_siblings() {
        let roots = vec![windows_root("D:\\Media")];
        let tracks = vec![
            input_at(&roots[0], &["Björk Guðmundsdóttir", "Debut", "1.flac"]),
            input_at(&roots[0], &["Artist", "Greatest Hits", "2.flac"]),
            input_at(&roots[0], &["Artist Deluxe", "Greatest Hits", "3.flac"]),
            input_at(&roots[0], &["Artist", "Rare", "4.flac"]),
        ];
        let (placed, _) = place_tracks(&roots, &tracks);
        let browser = FolderBrowser::new(roots, placed);

        let root_level = browser.children("D:\\Media", "").expect("root browses");
        assert_eq!(
            root_level,
            vec![
                child("Artist", "Artist", 2),
                child("Artist Deluxe", "Artist Deluxe", 1),
                child("Björk Guðmundsdóttir", "Björk Guðmundsdóttir", 1),
            ]
        );

        // Text-prefix siblings must not absorb each other's subtrees.
        assert_eq!(
            browser
                .children("D:\\Media", "Artist")
                .expect("Artist browses"),
            vec![
                child("Greatest Hits", "Artist/Greatest Hits", 1),
                child("Rare", "Artist/Rare", 1),
            ]
        );
        assert_eq!(
            browser
                .children("D:\\Media", "Artist Deluxe")
                .expect("Artist Deluxe browses"),
            vec![child("Greatest Hits", "Artist Deluxe/Greatest Hits", 1)]
        );
        assert_eq!(
            browser
                .children("D:\\Media", "Björk Guðmundsdóttir")
                .expect("unicode name browses"),
            vec![child("Debut", "Björk Guðmundsdóttir/Debut", 1)]
        );
    }

    #[test]
    fn folder_filter_prefix_is_native_and_containment_safe() {
        // The filter prefix joins onto native separators and keeps the
        // no-trailing-separator contract.
        assert_eq!(
            join_native("C:\\Music Library", "Artist/Album"),
            PathBuf::from("C:\\Music Library\\Artist\\Album")
        );
        assert_eq!(
            join_native("C:\\Music Library", ""),
            PathBuf::from("C:\\Music Library")
        );

        // A crafted dir cannot smuggle parent traversal past the clamp:
        // `\` inside a portable component is refused, and `..` pops with
        // root clamping, so nothing outside the root is nameable.
        assert_eq!(
            join_native("C:\\Music Library", "..\\..\\Windows"),
            PathBuf::from("C:\\Music Library")
        );
        assert_eq!(
            join_native("C:\\Music Library", "Artist/../../Windows"),
            PathBuf::from("C:\\Music Library\\Windows")
        );

        // And the URI form the window compares against track URIs holds
        // its containment contract: a track inside matches, a track under
        // a text-prefix sibling does not, and a file directly in the
        // browsed folder still matches.
        let prefix = uri_prefix(&join_native("C:\\Music Library", "Artist/Album"));
        let inside = uri_prefix(&PathBuf::from("C:\\Music Library\\Artist\\Album\\01.flac"));
        assert!(inside.starts_with(&prefix));
        let sibling = uri_prefix(&PathBuf::from(
            "C:\\Music Library\\Artist\\Album Deluxe\\9.flac",
        ));
        assert!(!sibling.starts_with(&prefix));
        let direct = uri_prefix(&PathBuf::from("C:\\Music Library\\Artist\\Album\\note.txt"));
        assert!(direct.starts_with(&prefix));
    }
}
