//! Playback-time resolution for local-library identities.
//!
//! Queue and playlist rows keep the exact SQLite `tracks.id`; this module is
//! the only boundary that turns that identity into retained filesystem
//! authority. Metadata or path matching is deliberately absent: those
//! heuristics belong to playlist reconciliation, never playback.

use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sea_orm::{DatabaseConnection, DbErr, EntityTrait};
use thiserror::Error;

use crate::architecture::media::MediaLease;
use crate::db::entities::{library_root, track};

use super::root_authority::{BoundFile, RootAuthorityLease};
pub use super::root_authority::{MountedMutationTarget, MountedRootAuthority};

/// A dead or remote filesystem must not leave GTK waiting indefinitely for a
/// local load decision. SQLite has its own five-second busy timeout; use the
/// same outer budget for the point-in-time file probe.
const FILE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of **speculative** retained-authority filesystem probes
/// (album-pane artwork lookups) that may run at once.
const MAX_CONCURRENT_AUTHORITY_PROBES: usize = 8;

/// Reserved capacity for **playback-critical** retained-authority probes.
///
/// Playback resolution must never be starved by speculative pane work, so it
/// draws on a gate of its own instead of sharing [`SPECULATIVE_PROBE_GATE`]
/// (2026-09-14 N5 review finding).
const RESERVED_PLAYBACK_PROBES: usize = 8;

/// Which consumer is acquiring retained-authority probe capacity.
///
/// The two classes draw on independent gates so that a saturated album-pane
/// lane cannot delay a playback resolution waiting for capacity
/// (2026-09-14 N5 review finding).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeClass {
    /// Speculative work (album-pane thumbnails). Bounded by
    /// [`SPECULATIVE_PROBE_GATE`] so recycled rows cannot accumulate
    /// blocking probes.
    Speculative,
    /// Playback-critical resolution from `play_current`. Draws on the
    /// reserved [`PLAYBACK_PROBE_GATE`] so pane saturation cannot starve it.
    Playback,
}

/// Bounds concurrent **speculative** retained-authority filesystem probes.
///
/// [`resolve_track`] submits its pre-extraction probe with
/// `tokio::task::spawn_blocking`. A blocking closure cannot be unwound once
/// it starts, and dropping its `JoinHandle` detaches rather than cancels it,
/// so a caller that cancels a pending resolution (the album pane's
/// `run_until_revoked`) would otherwise let recycled rows accumulate
/// expensive probes without bound on Tokio's shared blocking pool
/// (2026-09-14 review finding). The permit is acquired *before* the probe is
/// submitted and moved INTO the blocking closure, so it is released only
/// when the probe actually completes — never early on task abort — and at
/// most [`MAX_CONCURRENT_AUTHORITY_PROBES`] speculative probes can ever be in
/// flight.
static SPECULATIVE_PROBE_GATE: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_AUTHORITY_PROBES);

/// Reserved gate for playback-critical retained-authority probes.
///
/// Kept separate from [`SPECULATIVE_PROBE_GATE`] so that eight stuck
/// album-pane probes can never consume the capacity a playback resolution
/// needs (2026-09-14 N5 review finding). Playback is user-paced and
/// superseded one track at a time, so this bound is a safety cap on
/// blocking-pool growth rather than a scheduling choke point.
static PLAYBACK_PROBE_GATE: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(RESERVED_PLAYBACK_PROBES);

/// Select the retained-authority probe gate for `class`.
fn probe_gate(class: ProbeClass) -> &'static tokio::sync::Semaphore {
    match class {
        ProbeClass::Speculative => &SPECULATIVE_PROBE_GATE,
        ProbeClass::Playback => &PLAYBACK_PROBE_GATE,
    }
}

/// Acquire one retained-authority probe permit for an adapter that performs
/// its own blocking mounted probe (retained removable media).
///
/// Mirrors [`acquire_authority_probe`]'s discipline: the caller must move the
/// returned permit into the blocking closure so aborting the async caller
/// cannot release capacity while the probe still runs, and speculative
/// album-pane work draws on a different gate than playback so a saturated
/// pane lane cannot delay a playback resolution. `None` means the gate could
/// not be acquired within [`FILE_PROBE_TIMEOUT`]; the caller fails closed.
pub async fn acquire_retained_probe_permit(
    class: ProbeClass,
) -> Option<tokio::sync::SemaphorePermit<'static>> {
    let deadline = tokio::time::Instant::now() + FILE_PROBE_TIMEOUT;
    tokio::time::timeout_at(deadline, probe_gate(class).acquire())
        .await
        .ok()?
        .ok()
}

/// A closed, path-free local resolution failure safe for application logs.
#[derive(Debug, Error)]
pub enum LocalMediaResolutionError {
    #[error("local track identity is invalid")]
    InvalidTrackId,
    #[error("local track is no longer in the library")]
    Missing,
    #[error("local media database lookup failed")]
    Database {
        #[source]
        source: DbErr,
    },
    #[error("local track is outside the current configured library roots")]
    NoConfiguredRoot,
    #[error("local library root is not currently authoritative")]
    RootUnavailable,
    #[error("local media authority is unavailable")]
    AuthorityUnavailable {
        #[source]
        source: std::io::Error,
    },
    #[error("local media authority check timed out")]
    AuthorityCheckTimedOut,
    #[error("local track or root changed while media authority was acquired")]
    ChangedDuringResolution,
}

enum RetainedFileAuthority {
    Local {
        authority: Arc<RootAuthorityLease>,
        file: BoundFile,
        path: PathBuf,
    },
    Mounted {
        authority: Arc<MountedRootAuthority>,
        file: BoundFile,
    },
    OpenFile(File),
}

struct ResolvedLocalMediaInner {
    authority: RetainedFileAuthority,
    extension: Option<String>,
    seek_consumers: Mutex<()>,
}

/// Security-relevant database state captured before filesystem authority is
/// acquired.
///
/// `last_checked_at` is observational scan metadata, not an authority
/// generation: a concurrent successful scan may refresh it without changing
/// which root is trusted. Keep the comparison explicit so such timestamp-only
/// drift cannot spuriously reject an otherwise current resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedRootAuthorityState {
    path: String,
    device_id: Option<String>,
    identity_confirmed: bool,
    is_available: bool,
    last_scan_complete: bool,
}

impl ExpectedRootAuthorityState {
    fn from_model(state: &library_root::Model) -> Self {
        Self {
            path: state.path.clone(),
            device_id: state.device_id.clone(),
            identity_confirmed: state.identity_confirmed,
            is_available: state.is_available,
            last_scan_complete: state.last_scan_complete,
        }
    }

    fn matches(&self, state: &library_root::Model) -> bool {
        self.path == state.path
            && self.device_id == state.device_id
            && self.identity_confirmed == state.identity_confirmed
            && self.is_available == state.is_available
            && self.last_scan_complete == state.last_scan_complete
    }
}

/// Exact file authority retained for one resolved media use.
///
/// Local instances share the same root, marker, ancestor, and file handles;
/// mounted instances share their exact marker-free mount-root and descendant
/// handles; external instances share the exact already-open file object.
/// Outputs, their receiver-facing ticket servers, and the in-process
/// embedded-art reader keep a clone until their exact consumption finishes.
/// No consumer reopens a pathname.
#[derive(Clone)]
pub struct ResolvedLocalMedia {
    inner: Arc<ResolvedLocalMediaInner>,
    lease: Option<MediaLease>,
}

impl std::fmt::Debug for ResolvedLocalMedia {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedLocalMedia")
            .finish_non_exhaustive()
    }
}

impl ResolvedLocalMedia {
    /// Clone the already-authorized file object for a new local consumer.
    ///
    /// Platform clone operations may share a cursor with the retained handle;
    /// streaming consumers must use position-independent reads. Cursor-based
    /// parsers must use [`Self::with_serialized_seekable_file`] instead.
    pub(crate) fn try_clone_file(&self) -> std::io::Result<File> {
        self.require_active_lease()?;
        let file = match &self.inner.authority {
            RetainedFileAuthority::Local {
                authority, file, ..
            } => file.try_clone_for_consumption(authority)?,
            RetainedFileAuthority::Mounted { authority, file } => {
                file.try_clone_for_mounted_consumption(authority)?
            }
            RetainedFileAuthority::OpenFile(file) => {
                let cloned = file.try_clone()?;
                if !cloned.metadata()?.is_file() {
                    return Err(std::io::Error::other(
                        "retained media object is not a regular file",
                    ));
                }
                cloned
            }
        };
        self.require_active_lease()?;
        Ok(file)
    }

    /// Run one cursor-based consumer against a clone of this exact file.
    ///
    /// `File::try_clone` may share its seek cursor with sibling handles. The
    /// playback proxies use position-independent reads, while tag and artwork
    /// parsers require ordinary `Read + Seek`; serialize those parsers so two
    /// overlapping workers cannot disturb one another.
    pub(crate) fn with_serialized_seekable_file<T>(
        &self,
        consume: impl FnOnce(File) -> T,
    ) -> std::io::Result<T> {
        let _guard =
            self.inner.seek_consumers.lock().map_err(|_| {
                std::io::Error::other("retained media seek authority is unavailable")
            })?;
        let file = self.try_clone_file()?;
        Ok(consume(file))
    }

    /// Safe extension hint used to label an opaque media ticket and preserve
    /// the tag parser's format-specific behavior without exposing a path.
    pub(crate) fn extension(&self) -> Option<&str> {
        self.inner.extension.as_deref()
    }

    /// Confirm that this lease's root is still the most-specific configured
    /// root for the resolved path. The caller performs this path-only check on
    /// the GTK thread immediately before handing the lease to an output.
    pub(crate) fn matches_current_configuration(&self, configured_roots: &[String]) -> bool {
        let RetainedFileAuthority::Local {
            authority, path, ..
        } = &self.inner.authority
        else {
            return false;
        };
        configured_roots
            .iter()
            .map(PathBuf::from)
            .filter(|root| root.is_absolute() && path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .is_some_and(|root| root == authority.root())
    }

    /// Construct exact authority from a file object already opened and
    /// validated by the caller's operating-system delivery boundary.
    ///
    /// The pathname is deliberately absent. The regular-file check occurs
    /// before the capability can enter an adapter, and every later clone
    /// repeats it against the retained object itself.
    pub(crate) fn from_open_regular_file(
        file: File,
        extension: Option<String>,
    ) -> std::io::Result<Self> {
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other(
                "external media object is not a regular file",
            ));
        }
        Ok(Self {
            inner: Arc::new(ResolvedLocalMediaInner {
                authority: RetainedFileAuthority::OpenFile(file),
                extension,
                seek_consumers: Mutex::new(()),
            }),
            lease: None,
        })
    }

    /// Resolve one native relative path beneath an exact retained mount root.
    ///
    /// The path is consumed only inside the retained filesystem authority and
    /// is never exposed as a playable locator. Every later handle clone
    /// revalidates the mounted root, its filesystem boundary, ancestor chain,
    /// exact file object, and optional lifecycle lease.
    pub(crate) fn from_mounted_relative_path(
        authority: Arc<MountedRootAuthority>,
        relative_path: &std::path::Path,
        extension: Option<String>,
    ) -> std::io::Result<Self> {
        let file = authority.open_relative_regular_file(relative_path)?;
        Ok(Self {
            inner: Arc::new(ResolvedLocalMediaInner {
                authority: RetainedFileAuthority::Mounted { authority, file },
                extension,
                seek_consumers: Mutex::new(()),
            }),
            lease: None,
        })
    }

    /// Attach the lifecycle lease that owns this exact file capability.
    pub(crate) fn with_lease(mut self, lease: MediaLease) -> Self {
        self.lease = Some(lease);
        self
    }

    pub(crate) fn is_active(&self) -> bool {
        self.lease.as_ref().is_none_or(MediaLease::is_active)
    }

    fn require_active_lease(&self) -> std::io::Result<()> {
        if self.is_active() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "retained media source is no longer active",
            ))
        }
    }

    /// Construct retained local authority without a database for focused
    /// output-boundary tests.
    #[cfg(test)]
    pub(crate) fn from_authorized_path_for_test(
        root: &std::path::Path,
        expected_marker: &str,
        path: &std::path::Path,
    ) -> std::io::Result<Self> {
        let authority = Arc::new(RootAuthorityLease::acquire(root, expected_marker)?);
        let file = authority.open_regular_file(path)?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_owned);
        Ok(Self {
            inner: Arc::new(ResolvedLocalMediaInner {
                authority: RetainedFileAuthority::Local {
                    authority,
                    file,
                    path: path.to_path_buf(),
                },
                extension,
                seek_consumers: Mutex::new(()),
            }),
            lease: None,
        })
    }
}

/// General name for the exact retained file capability shared by local and
/// lifecycle-owned filesystem adapters.
pub type ResolvedFileMedia = ResolvedLocalMedia;

fn configured_root_states<'a>(
    states: &'a [library_root::Model],
    configured_roots: &[String],
) -> Vec<(&'a library_root::Model, PathBuf)> {
    let configured: Vec<PathBuf> = configured_roots
        .iter()
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .collect();
    let mut matching: Vec<_> = states
        .iter()
        .filter_map(|state| {
            let root = PathBuf::from(&state.path);
            configured
                .iter()
                .any(|configured| configured == &root)
                .then_some((state, root))
        })
        .collect();
    matching.sort_by_key(|(_, root)| std::cmp::Reverse(root.components().count()));
    matching
}

/// Resolve one exact source-native local track ID against the current database
/// row and filesystem immediately before an output load.
///
/// The lookup never falls back to a captured path, playlist fingerprint,
/// metadata, or another track. A row removed after queue creation is therefore
/// unavailable, while a committed rename is observed without rewriting the
/// queue. The returned value retains root, marker, ancestor, and exact file
/// handles for the complete output/ticket lifecycle.
pub async fn resolve_track(
    db: &DatabaseConnection,
    track_id: &str,
    configured_roots: &[String],
) -> Result<ResolvedLocalMedia, LocalMediaResolutionError> {
    resolve_track_with_class(ProbeClass::Speculative, db, track_id, configured_roots).await
}

/// Resolve one local track using an explicit [`ProbeClass`].
///
/// [`resolve_track`] is the speculative convenience wrapper used by
/// album-pane artwork. Playback-critical callers pass
/// [`ProbeClass::Playback`] so their probe capacity is reserved and cannot be
/// starved by pane work (2026-09-14 N5 review finding).
pub async fn resolve_track_with_class(
    class: ProbeClass,
    db: &DatabaseConnection,
    track_id: &str,
    configured_roots: &[String],
) -> Result<ResolvedLocalMedia, LocalMediaResolutionError> {
    if track_id.is_empty() {
        return Err(LocalMediaResolutionError::InvalidTrackId);
    }

    let model = load_track(db, track_id).await?;
    let AuthorizedRoot {
        expected,
        root,
        marker,
    } = select_authorized_root(db, &model.file_path, configured_roots).await?;
    let path = PathBuf::from(&model.file_path);
    let acquired = acquire_authority_probe(class, track_id, &root, &path, &marker).await?;

    // The blocking handle acquisition is intentionally outside SQLite. Re-read
    // both bindings afterward so a concurrent reconciliation/root demotion
    // cannot publish authority acquired for an obsolete database snapshot.
    verify_unchanged(db, track_id, &model.file_path, &expected).await?;

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_owned);

    Ok(ResolvedLocalMedia {
        inner: Arc::new(ResolvedLocalMediaInner {
            authority: RetainedFileAuthority::Local {
                authority: acquired.0,
                file: acquired.1,
                path,
            },
            extension,
            seek_consumers: Mutex::new(()),
        }),
        lease: None,
    })
}

/// The configured root that currently owns a track, captured together with the
/// exact authority identity a probe must revalidate.
struct AuthorizedRoot {
    expected: ExpectedRootAuthorityState,
    root: PathBuf,
    marker: String,
}

/// Load the exact track row or report it as missing.
async fn load_track(
    db: &DatabaseConnection,
    track_id: &str,
) -> Result<track::Model, LocalMediaResolutionError> {
    track::Entity::find_by_id(track_id.to_string())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::Missing)
}

/// Select the authoritative configured root containing `file_path`.
///
/// Fails closed when no configured root contains the path, when the owning
/// root is not currently authoritative, or when its identity marker is
/// absent. The returned [`ExpectedRootAuthorityState`] captures the row so a
/// later re-read can detect a concurrent demotion.
async fn select_authorized_root(
    db: &DatabaseConnection,
    file_path: &str,
    configured_roots: &[String],
) -> Result<AuthorizedRoot, LocalMediaResolutionError> {
    let states = library_root::Entity::find()
        .all(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?;
    let path = PathBuf::from(file_path);
    let Some((state, root)) = configured_root_states(&states, configured_roots)
        .into_iter()
        .find(|(_, root)| path.starts_with(root))
    else {
        return Err(LocalMediaResolutionError::NoConfiguredRoot);
    };
    if !state.identity_confirmed || !state.is_available || !state.last_scan_complete {
        return Err(LocalMediaResolutionError::RootUnavailable);
    }
    let marker = state
        .device_id
        .clone()
        .ok_or(LocalMediaResolutionError::RootUnavailable)?;
    Ok(AuthorizedRoot {
        expected: ExpectedRootAuthorityState::from_model(state),
        root,
        marker,
    })
}

/// Acquire retained filesystem authority for `path` under `root`.
///
/// Bound the probe before submitting it. A cancelled caller must not be able
/// to free gate capacity while its detached blocking closure still runs, so
/// the permit is moved into the closure below. The existing five-second budget
/// covers both waiting for capacity and running the probe: a saturated gate
/// can delay a resolution but never stretch it past `FILE_PROBE_TIMEOUT`, and
/// a callback whose row was recycled while it queued is dropped before it ever
/// submits a blocking probe.
///
/// Speculative (album-pane) and playback-critical callers acquire from
/// independent gates, so a panes-only saturation cannot delay a playback
/// resolution here (2026-09-14 N5 review finding).
#[cfg_attr(not(test), allow(unused_variables))]
async fn acquire_authority_probe(
    class: ProbeClass,
    track_id: &str,
    root: &std::path::Path,
    path: &std::path::Path,
    marker: &str,
) -> Result<(Arc<RootAuthorityLease>, BoundFile), LocalMediaResolutionError> {
    let deadline = tokio::time::Instant::now() + FILE_PROBE_TIMEOUT;
    let probe_permit = tokio::time::timeout_at(deadline, probe_gate(class).acquire())
        .await
        .map_err(|_| LocalMediaResolutionError::AuthorityCheckTimedOut)?
        .map_err(|_| LocalMediaResolutionError::AuthorityUnavailable {
            source: std::io::Error::other("local authority probe gate unavailable"),
        })?;
    let authority_path = path.to_path_buf();
    let authority_root = root.to_path_buf();
    let expected_marker = marker.to_owned();
    #[cfg(test)]
    let probe_track_id = track_id.to_string();
    tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            // Hold the gate permit for the entire blocking closure so an
            // aborted async caller cannot release it while this probe is
            // still queued or running (2026-09-14 review finding).
            let _probe_permit = probe_permit;
            #[cfg(test)]
            let probe_park_guard = probe_park::enter_if_watched(&probe_track_id);
            #[cfg(test)]
            if let Some(park) = probe_park_guard.as_ref() {
                park.wait_for_release();
            }
            let authority = Arc::new(RootAuthorityLease::acquire(
                &authority_root,
                &expected_marker,
            )?);
            let file = authority.open_regular_file(&authority_path)?;
            Ok::<_, std::io::Error>((authority, file))
        }),
    )
    .await
    .map_err(|_| LocalMediaResolutionError::AuthorityCheckTimedOut)?
    .map_err(|source| LocalMediaResolutionError::AuthorityUnavailable {
        source: std::io::Error::other(format!("local authority task failed: {source}")),
    })?
    .map_err(|source| LocalMediaResolutionError::AuthorityUnavailable { source })
}

/// Re-read the track and owning root after the blocking probe.
///
/// The blocking handle acquisition is intentionally outside SQLite. Re-reading
/// both bindings afterward ensures a concurrent reconciliation/root demotion
/// cannot publish authority acquired for an obsolete database snapshot.
async fn verify_unchanged(
    db: &DatabaseConnection,
    track_id: &str,
    file_path: &str,
    expected: &ExpectedRootAuthorityState,
) -> Result<(), LocalMediaResolutionError> {
    let current_model = track::Entity::find_by_id(track_id.to_string())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::Missing)?;
    let current_state = library_root::Entity::find_by_id(expected.path.clone())
        .one(db)
        .await
        .map_err(|source| LocalMediaResolutionError::Database { source })?
        .ok_or(LocalMediaResolutionError::ChangedDuringResolution)?;
    if current_model.file_path != file_path || !expected.matches(&current_state) {
        return Err(LocalMediaResolutionError::ChangedDuringResolution);
    }
    Ok(())
}

/// Test-only instrumentation for the retained-authority probe gates.
///
/// Records how many watched probe closures are concurrently executing and
/// parks them behind a condvar, so a regression can observe the bound
/// deterministically through the real [`resolve_track`] seam. Production
/// builds compile this module away entirely.
#[cfg(test)]
mod probe_park {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    struct Park {
        enabled: AtomicBool,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        watcher: Mutex<Option<String>>,
        waiting: Mutex<()>,
        release: Condvar,
        /// Deterministic lost-wakeup regression control: when armed, the
        /// first waiter that reads `enabled == true` under `waiting` pauses
        /// inside that window -- still holding `waiting` -- until poked,
        /// simulating preemption between the predicate read and the condvar
        /// registration where an unsynchronized [`release`](fn.release)
        /// used to be lost (2026-09-17 audit rejection).
        gap: Mutex<Gap>,
        gap_signal: Condvar,
    }

    /// State of the predicate-to-wait interleaving window.
    #[derive(Default)]
    struct Gap {
        /// Arm the window for the next waiter.
        armed: bool,
        /// A waiter currently sits in the window, holding `waiting`.
        occupied: bool,
        /// The tester poked: the waiter must leave the window.
        poke: bool,
    }

    fn park() -> &'static Park {
        static PARK: OnceLock<Park> = OnceLock::new();
        PARK.get_or_init(|| Park {
            enabled: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            watcher: Mutex::new(None),
            waiting: Mutex::new(()),
            release: Condvar::new(),
            gap: Mutex::new(Gap::default()),
            gap_signal: Condvar::new(),
        })
    }

    /// Watch exactly `track_id` and park its probe closures until released.
    pub(super) fn watch(track_id: &str) {
        let state = park();
        *state.watcher.lock().expect("probe park watcher") = Some(track_id.to_string());
        state.in_flight.store(0, Ordering::SeqCst);
        state.peak.store(0, Ordering::SeqCst);
        // Arm under the same mutex `release` disarms under, so the two
        // transitions can never interleave with a waiter's predicate loop.
        let wait_lock = state.waiting.lock().expect("probe park watch");
        state.enabled.store(true, Ordering::SeqCst);
        drop(wait_lock);
    }

    /// Disarm the park and wake every parked closure.
    pub(super) fn release() {
        let state = park();
        // Flip `enabled` while holding the waiter mutex. A waiter that has
        // read `enabled == true` under that mutex either sees the flip when
        // it re-checks, or has already registered on the condvar (its
        // `wait` released the mutex to us), so the trailing notification
        // can never be lost. Without this lock the flip lands between the
        // waiter's last true predicate read and its wait registration: the
        // parked probe never wakes, and the test runtime waiting on it
        // hangs forever (2026-09-17 audit rejection).
        let wait_lock = state.waiting.lock().expect("probe park release");
        state.enabled.store(false, Ordering::SeqCst);
        drop(wait_lock);
        state.release.notify_all();
    }

    /// Arm the predicate-to-wait window for the next waiter (regression
    /// control; inert for every other test).
    pub(super) fn arm_gap() {
        let state = park();
        *state.gap.lock().expect("probe park gap") = Gap {
            armed: true,
            occupied: false,
            poke: false,
        };
    }

    /// Wait (bounded) until a waiter sits inside the predicate-to-wait
    /// window. Returns `false` when the deadline passes without occupation.
    pub(super) fn wait_for_gap_occupation(budget: Duration) -> bool {
        let state = park();
        let deadline = Instant::now() + budget;
        loop {
            if state.gap.lock().expect("probe park gap").occupied {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Free whichever waiter occupies the window and disarm it. Safe to
    /// call when the window was never entered.
    pub(super) fn poke_gap() {
        let state = park();
        let mut gap = state.gap.lock().expect("probe park gap");
        gap.poke = true;
        state.gap_signal.notify_all();
    }

    pub(super) fn in_flight() -> usize {
        park().in_flight.load(Ordering::SeqCst)
    }

    pub(super) fn peak() -> usize {
        park().peak.load(Ordering::SeqCst)
    }

    fn is_watched(track_id: &str) -> bool {
        park()
            .watcher
            .lock()
            .map(|watcher| watcher.as_deref() == Some(track_id))
            .unwrap_or(false)
    }

    /// Enter one watched probe, returning a guard that keeps the in-flight
    /// count and parks the closure until [`release`]. Unwatched probes get
    /// `None` and pay nothing.
    pub(super) fn enter_if_watched(track_id: &str) -> Option<Guard> {
        if !is_watched(track_id) {
            return None;
        }
        let state = park();
        let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(now, Ordering::SeqCst);
        Some(Guard { state })
    }

    pub(super) struct Guard {
        state: &'static Park,
    }

    impl Guard {
        pub(super) fn wait_for_release(&self) {
            if !self.state.enabled.load(Ordering::SeqCst) {
                return;
            }
            let mut guard = self.state.waiting.lock().expect("probe park wait");
            while self.state.enabled.load(Ordering::SeqCst) {
                self.state.stall_in_gap_window();
                guard = self
                    .state
                    .release
                    .wait(guard)
                    .expect("probe park wait poisoned");
            }
        }
    }

    impl Park {
        /// Pause inside the predicate-to-wait window -- `waiting` is held
        /// for the whole call -- until the tester pokes the window. No-op
        /// unless the window was armed for this waiter.
        fn stall_in_gap_window(&self) {
            let mut gap = self.gap.lock().expect("probe park gap");
            if !gap.armed {
                return;
            }
            gap.occupied = true;
            self.gap_signal.notify_all();
            while !gap.poke {
                gap = self.gap_signal.wait(gap).expect("probe park gap poisoned");
            }
            *gap = Gap::default();
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

    use super::*;
    use crate::db::migration::Migrator;

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open test database");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    fn model(id: &str, path: &Path) -> track::ActiveModel {
        track::ActiveModel {
            id: Set(id.to_string()),
            file_path: Set(path.to_string_lossy().into_owned()),
            title: Set("Title".to_string()),
            artist_name: Set("Artist".to_string()),
            album_title: Set("Album".to_string()),
            play_count: Set(0),
            last_played_at_ms: Set(None),
            date_added: Set("2026-07-17T00:00:00+00:00".to_string()),
            date_modified: Set("2026-07-17T00:00:00+00:00".to_string()),
            ..Default::default()
        }
    }

    async fn authorize_root(db: &DatabaseConnection, root: &Path) -> String {
        let marker = format!("marker:v1:{}", Uuid::new_v4());
        std::fs::write(root.join(".tributary-root-id"), format!("{marker}\n"))
            .expect("write root marker");
        library_root::ActiveModel {
            path: Set(root.to_string_lossy().into_owned()),
            device_id: Set(Some(marker.clone())),
            identity_confirmed: Set(true),
            is_available: Set(true),
            last_scan_complete: Set(true),
            last_checked_at: Set("2026-07-17T00:00:00+00:00".to_string()),
        }
        .insert(db)
        .await
        .expect("insert authoritative root");
        marker
    }

    fn configured(root: &Path) -> Vec<String> {
        vec![root.to_string_lossy().into_owned()]
    }

    fn read_media(media: &ResolvedLocalMedia) -> Vec<u8> {
        let mut file = media.try_clone_file().expect("clone authorized file");
        file.seek(SeekFrom::Start(0))
            .expect("reset shared test cursor");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read authorized file");
        bytes
    }

    fn local_media_path(media: &ResolvedLocalMedia) -> &Path {
        match &media.inner.authority {
            RetainedFileAuthority::Local { path, .. } => path,
            RetainedFileAuthority::Mounted { .. } | RetainedFileAuthority::OpenFile(_) => {
                panic!("fixture expected local authority")
            }
        }
    }

    #[test]
    fn cursor_based_consumers_are_serialized_for_one_retained_file() {
        let directory = tempfile::tempdir().expect("retained media directory");
        let path = directory.path().join("song.bin");
        std::fs::write(&path, b"retained media").expect("write retained media");
        let media = ResolvedLocalMedia::from_open_regular_file(
            File::open(&path).expect("open retained media"),
            Some("bin".to_string()),
        )
        .expect("construct retained media");

        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_media = media.clone();
        let first = thread::spawn(move || {
            first_media
                .with_serialized_seekable_file(|_file| {
                    first_entered_tx.send(()).expect("report first entry");
                    release_first_rx.recv().expect("release first consumer");
                })
                .expect("run first seek consumer");
        });
        first_entered_rx.recv().expect("first consumer entered");

        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            media
                .with_serialized_seekable_file(|_file| {
                    second_entered_tx.send(()).expect("report second entry");
                })
                .expect("run second seek consumer");
        });
        assert_eq!(
            second_entered_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        release_first_tx.send(()).expect("release first consumer");
        first.join().expect("join first consumer");
        second_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second consumer entered after release");
        second.join().expect("join second consumer");
    }

    #[test]
    fn mounted_media_retains_the_exact_file_and_observes_lease_revocation() {
        let directory = tempfile::tempdir().expect("mounted media directory");
        let path = directory.path().join("song.flac");
        let displaced = directory.path().join("original.flac");
        std::fs::write(&path, b"mounted original").expect("write mounted media");
        let authority = Arc::new(
            MountedRootAuthority::acquire(directory.path()).expect("acquire mounted authority"),
        );
        let media = ResolvedLocalMedia::from_mounted_relative_path(
            authority,
            Path::new("song.flac"),
            Some("flac".to_string()),
        )
        .expect("resolve mounted media");
        assert_eq!(media.extension(), Some("flac"));
        assert!(!media.matches_current_configuration(&configured(directory.path())));

        std::fs::rename(&path, &displaced).expect("displace mounted media");
        std::fs::write(&path, b"mounted replacement").expect("replace mounted media");
        assert_eq!(read_media(&media), b"mounted original");

        let lease = MediaLease::new();
        let leased = media.with_lease(lease.clone());
        assert_eq!(read_media(&leased), b"mounted original");
        lease.revoke();
        assert!(leased.try_clone_file().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn mounted_media_fails_closed_after_root_replacement() {
        let directory = tempfile::tempdir().expect("mounted media directory");
        let replacement = tempfile::tempdir().expect("replacement mounted directory");
        std::fs::write(directory.path().join("song.flac"), b"mounted original")
            .expect("write mounted media");
        std::fs::write(replacement.path().join("song.flac"), b"replacement")
            .expect("write replacement media");
        let authority = Arc::new(
            MountedRootAuthority::acquire(directory.path()).expect("acquire mounted authority"),
        );
        let media = ResolvedLocalMedia::from_mounted_relative_path(
            authority,
            Path::new("song.flac"),
            Some("flac".to_string()),
        )
        .expect("resolve mounted media");
        let displaced = directory.path().with_extension("displaced");
        std::fs::rename(directory.path(), &displaced).expect("displace mounted root");
        std::fs::rename(replacement.path(), directory.path()).expect("install replacement root");

        assert!(media.try_clone_file().is_err());

        drop(media);
        std::fs::rename(directory.path(), replacement.path()).expect("restore replacement root");
        std::fs::rename(&displaced, directory.path()).expect("restore mounted root");
    }

    #[test]
    fn root_authority_snapshot_ignores_timestamp_but_binds_every_authority_field() {
        let original = library_root::Model {
            path: "/music".to_string(),
            device_id: Some("marker:v1:00000000-0000-4000-8000-000000000000".to_string()),
            identity_confirmed: true,
            is_available: true,
            last_scan_complete: true,
            last_checked_at: "2026-07-17T00:00:00Z".to_string(),
        };
        let expected = ExpectedRootAuthorityState::from_model(&original);

        let mut timestamp_only = original.clone();
        timestamp_only.last_checked_at = "2099-01-01T00:00:00Z".to_string();
        assert!(expected.matches(&timestamp_only));

        let mut changed = original.clone();
        changed.path = "/replacement".to_string();
        assert!(!expected.matches(&changed));

        let mut changed = original.clone();
        changed.device_id = Some("marker:v1:ffffffff-ffff-4fff-bfff-ffffffffffff".to_string());
        assert!(!expected.matches(&changed));

        let mut changed = original.clone();
        changed.identity_confirmed = false;
        assert!(!expected.matches(&changed));

        let mut changed = original.clone();
        changed.is_available = false;
        assert!(!expected.matches(&changed));

        let mut changed = original;
        changed.last_scan_complete = false;
        assert!(!expected.matches(&changed));
    }

    #[tokio::test]
    async fn exact_non_uuid_id_observes_a_committed_rename_at_use() {
        let db = database().await;
        let directory = tempfile::tempdir().expect("temporary media directory");
        let old_path = directory.path().join("old.flac");
        let new_path = directory.path().join("renamed.flac");
        std::fs::write(&old_path, b"old").expect("write old fixture");
        std::fs::write(&new_path, b"new").expect("write renamed fixture");
        authorize_root(&db, directory.path()).await;
        let roots = configured(directory.path());

        model("legacy:not-a-uuid", &old_path)
            .insert(&db)
            .await
            .expect("insert exact legacy ID");

        let old_media = resolve_track(&db, "legacy:not-a-uuid", &roots)
            .await
            .expect("resolve original row");
        assert_eq!(local_media_path(&old_media), old_path);
        assert_eq!(read_media(&old_media), b"old");

        let mut active: track::ActiveModel = track::Entity::find_by_id("legacy:not-a-uuid")
            .one(&db)
            .await
            .expect("query row")
            .expect("row exists")
            .into();
        active.file_path = Set(new_path.to_string_lossy().into_owned());
        active.update(&db).await.expect("commit rename");

        let current_media = resolve_track(&db, "legacy:not-a-uuid", &roots)
            .await
            .expect("resolve renamed row");
        assert_eq!(local_media_path(&current_media), new_path);
        assert_eq!(read_media(&current_media), b"new");
    }

    #[tokio::test]
    async fn resolution_never_falls_back_for_invalid_missing_or_dead_ids() {
        let db = database().await;
        let directory = tempfile::tempdir().expect("temporary media directory");
        let dead_path = directory.path().join("gone.flac");
        authorize_root(&db, directory.path()).await;
        let roots = configured(directory.path());
        model("dead-track", &dead_path)
            .insert(&db)
            .await
            .expect("insert dead track");

        assert!(matches!(
            resolve_track(&db, "", &roots).await,
            Err(LocalMediaResolutionError::InvalidTrackId)
        ));
        assert!(matches!(
            resolve_track(&db, "different-track", &roots).await,
            Err(LocalMediaResolutionError::Missing)
        ));
        let dead = resolve_track(&db, "dead-track", &roots)
            .await
            .expect_err("dead path must fail");
        assert!(matches!(
            &dead,
            LocalMediaResolutionError::AuthorityUnavailable { .. }
        ));
        let rendered = dead.to_string();
        assert_eq!(rendered, "local media authority is unavailable");
        assert!(!rendered.contains("dead-track"));
        assert!(!rendered.contains("gone.flac"));
    }

    #[tokio::test]
    async fn current_config_and_most_specific_root_fail_closed() {
        let db = database().await;
        let parent = tempfile::tempdir().expect("parent root");
        let child = parent.path().join("child");
        std::fs::create_dir(&child).expect("create child root");
        authorize_root(&db, parent.path()).await;
        authorize_root(&db, &child).await;
        let path = child.join("track.flac");
        std::fs::write(&path, b"track").expect("write track");
        model("track", &path)
            .insert(&db)
            .await
            .expect("insert track");

        assert!(matches!(
            resolve_track(&db, "track", &[]).await,
            Err(LocalMediaResolutionError::NoConfiguredRoot)
        ));

        let mut child_state: library_root::ActiveModel =
            library_root::Entity::find_by_id(child.to_string_lossy().into_owned())
                .one(&db)
                .await
                .expect("query child state")
                .expect("child state exists")
                .into();
        child_state.is_available = Set(false);
        child_state.update(&db).await.expect("demote child root");
        let both = vec![
            parent.path().to_string_lossy().into_owned(),
            child.to_string_lossy().into_owned(),
        ];
        assert!(matches!(
            resolve_track(&db, "track", &both).await,
            Err(LocalMediaResolutionError::RootUnavailable)
        ));

        let parent_only = configured(parent.path());
        let media = resolve_track(&db, "track", &parent_only)
            .await
            .expect("the explicitly configured parent remains authoritative");
        assert!(media.matches_current_configuration(&parent_only));
        assert!(!media.matches_current_configuration(&configured(&child)));
        assert!(
            !media.matches_current_configuration(&both),
            "adding a more-specific configured root invalidates a parent-root result"
        );
    }

    #[tokio::test]
    async fn admitted_media_reads_the_retained_file_not_a_path_replacement() {
        let db = database().await;
        let root = tempfile::tempdir().expect("library root");
        authorize_root(&db, root.path()).await;
        let roots = configured(root.path());
        let path = root.path().join("track.flac");
        let displaced = root.path().join("displaced.flac");
        std::fs::write(&path, b"authorized").expect("write authorized file");
        model("track", &path)
            .insert(&db)
            .await
            .expect("insert track");

        let media = resolve_track(&db, "track", &roots)
            .await
            .expect("resolve exact file");
        let replacement_installed = match std::fs::rename(&path, &displaced) {
            Ok(()) => {
                std::fs::write(&path, b"replacement").expect("install path replacement");
                true
            }
            Err(error) => {
                #[cfg(not(windows))]
                panic!("move admitted file: {error}");
                #[cfg(windows)]
                {
                    let _ = error;
                    // Windows authority handles intentionally omit delete
                    // sharing. A blocked rename is the platform's stronger
                    // form of the same guarantee: the admitted name cannot be
                    // retargeted while held.
                    assert_eq!(
                        std::fs::read(&path).expect("read pinned path"),
                        b"authorized"
                    );
                    false
                }
            }
        };

        assert_eq!(read_media(&media), b"authorized");
        if replacement_installed {
            assert_ne!(
                read_media(&media),
                std::fs::read(&path).expect("read replacement")
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_never_authorized_by_a_textual_root_prefix() {
        use std::os::unix::fs::symlink;

        let db = database().await;
        let root = tempfile::tempdir().expect("library root");
        let outside = tempfile::tempdir().expect("outside directory");
        authorize_root(&db, root.path()).await;
        let outside_file = outside.path().join("outside.flac");
        std::fs::write(&outside_file, b"outside").expect("write outside file");
        let link = root.path().join("escape");
        symlink(outside.path(), &link).expect("create directory symlink");
        let escaped_path = link.join("outside.flac");
        model("escaped", &escaped_path)
            .insert(&db)
            .await
            .expect("insert escaped path");

        assert!(matches!(
            resolve_track(&db, "escaped", &configured(root.path())).await,
            Err(LocalMediaResolutionError::AuthorityUnavailable { .. })
        ));
    }

    /// Release the process-global probe park even when an assertion unwinds.
    struct ParkGuard;

    impl Drop for ParkGuard {
        fn drop(&mut self) {
            probe_park::release();
        }
    }

    /// Free a waiter held inside the `probe_park` predicate-to-wait gap even
    /// when an assertion unwinds, so a failing test can never strand
    /// `release()` behind a held wait mutex.
    struct GapPokeGuard;

    impl Drop for GapPokeGuard {
        fn drop(&mut self) {
            probe_park::poke_gap();
        }
    }

    /// Serializes tests that drive the process-global `probe_park`
    /// instrumentation: only one watcher can be armed at a time, and sibling
    /// tests run in parallel.
    static PROBE_PARK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Spawn `attempt_count` real `resolve_track` calls for `track`, returning
    /// their handles so the caller can simulate recycled rows by aborting them.
    fn spawn_resolution_attempts(
        db: &Arc<DatabaseConnection>,
        roots: &[String],
        track: &str,
        attempt_count: usize,
    ) -> Vec<tokio::task::JoinHandle<Result<ResolvedLocalMedia, LocalMediaResolutionError>>> {
        (0..attempt_count)
            .map(|_| {
                let db = Arc::clone(db);
                let roots = roots.to_vec();
                let track = track.to_string();
                tokio::spawn(async move { resolve_track(&db, &track, &roots).await })
            })
            .collect()
    }

    /// Wait until the watched probe count reaches `target`, panicking with
    /// `message` if it does not within the two-second budget.
    async fn wait_for_probe_count(target: usize, message: &'static str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while probe_park::in_flight() != target {
            assert!(tokio::time::Instant::now() < deadline, "{message}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn recycled_rows_cannot_grow_the_authority_probe_backlog_past_the_bound() {
        let _serial = PROBE_PARK_TEST_LOCK.lock().await;
        let db = database().await;
        let root = tempfile::tempdir().expect("library root");
        authorize_root(&db, root.path()).await;
        let roots = configured(root.path());
        let path = root.path().join("track.flac");
        std::fs::write(&path, b"authorized").expect("write authorized file");
        // `probe_park` is process-global, so watch a private id: sibling
        // tests run in parallel and resolve their own tracks.
        const TRACK: &str = "bounded-probe-track";
        model(TRACK, &path).insert(&db).await.expect("insert track");

        let db = Arc::new(db);
        probe_park::watch(TRACK);
        let _park = ParkGuard;

        // Submit several times the permitted concurrency, then let the
        // resolutions reach the point where their blocking probes run.
        let attempts =
            spawn_resolution_attempts(&db, &roots, TRACK, MAX_CONCURRENT_AUTHORITY_PROBES * 3);

        // The gate is the only thing that can keep the rest of the probes
        // out; wait until every permit is held by a parked probe.
        wait_for_probe_count(
            MAX_CONCURRENT_AUTHORITY_PROBES,
            "fewer than the bound of probes entered the gate",
        )
        .await;
        assert_eq!(
            probe_park::peak(),
            MAX_CONCURRENT_AUTHORITY_PROBES,
            "the gate must cap concurrent authority probes at the declared bound"
        );

        // Recycled rows abandon their resolution (the album pane's
        // `run_until_revoked` drops the pending future). The probe already
        // running is detached and keeps its permit, so cancelling the async
        // callers must not hand that capacity to a new probe.
        for attempt in &attempts {
            attempt.abort();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            probe_park::in_flight(),
            MAX_CONCURRENT_AUTHORITY_PROBES,
            "a cancelled caller must not release capacity held by a running probe"
        );

        probe_park::release();
        wait_for_probe_count(0, "parked probes did not finish after release").await;
        assert_eq!(
            probe_park::peak(),
            MAX_CONCURRENT_AUTHORITY_PROBES,
            "the backlog never exceeded the bound"
        );
    }

    /// The speculative pane lane and the playback lane must draw on separate
    /// capacity: a fully saturated album-pane lane must not delay a playback
    /// resolution for an unrelated healthy root (2026-09-14 N5 review
    /// finding).
    #[tokio::test]
    async fn saturated_pane_probes_do_not_block_a_playback_resolution() {
        let _serial = PROBE_PARK_TEST_LOCK.lock().await;
        let db = database().await;
        let root = tempfile::tempdir().expect("library root");
        authorize_root(&db, root.path()).await;
        let roots = configured(root.path());
        let pane_path = root.path().join("pane.flac");
        std::fs::write(&pane_path, b"pane").expect("write pane file");
        let playback_path = root.path().join("playback.flac");
        std::fs::write(&playback_path, b"playback").expect("write playback file");
        // `probe_park` is process-global, so watch a private id; the playback
        // track is deliberately a different id so its probe is not parked.
        const PANE_TRACK: &str = "saturated-pane-track";
        const PLAYBACK_TRACK: &str = "starving-playback-track";
        model(PANE_TRACK, &pane_path)
            .insert(&db)
            .await
            .expect("insert pane track");
        model(PLAYBACK_TRACK, &playback_path)
            .insert(&db)
            .await
            .expect("insert playback track");

        let db = Arc::new(db);
        probe_park::watch(PANE_TRACK);
        let _park = ParkGuard;

        // Saturate the speculative lane: every permit is held by a parked
        // pane probe, each of which is detached from its async caller.
        let pane_attempts =
            spawn_resolution_attempts(&db, &roots, PANE_TRACK, MAX_CONCURRENT_AUTHORITY_PROBES);
        wait_for_probe_count(
            MAX_CONCURRENT_AUTHORITY_PROBES,
            "the pane lane did not saturate",
        )
        .await;

        // A playback resolution for a different healthy track must still
        // reach the filesystem promptly rather than waiting out the probe
        // budget for a permit held by pane work.
        let playback = tokio::time::timeout(
            Duration::from_secs(2),
            resolve_track_with_class(ProbeClass::Playback, &db, PLAYBACK_TRACK, &roots),
        )
        .await
        .expect("a playback resolution must not be starved by saturated pane probes");
        assert!(
            playback.is_ok(),
            "playback resolution should succeed: {playback:?}"
        );

        probe_park::release();
        for attempt in pane_attempts {
            let resolved = attempt.await.expect("parked pane probe joined");
            assert!(
                resolved.is_ok(),
                "saturated pane probe should succeed: {resolved:?}"
            );
        }
        wait_for_probe_count(0, "parked pane probes did not finish after release").await;
    }

    /// The 2026-09-17 audit rejection: `release()` used to flip `enabled`
    /// and notify without acquiring the waiter mutex, so a release landing
    /// between the waiter's last true predicate read and its
    /// `Condvar::wait` registration was lost and the parked probe -- plus
    /// the test runtime joining it -- hung forever. Force that exact
    /// interleaving deterministically: the waiter pauses inside the
    /// predicate-to-wait window while holding the wait mutex, and release
    /// must block behind it, complete once the waiter registers, and wake
    /// it. Bounded even on failure: the poke guard drops before the park
    /// guard, so a failing assertion can never strand `release()` behind a
    /// held wait mutex.
    #[tokio::test]
    async fn release_landing_in_the_predicate_to_wait_gap_wakes_the_parked_probe() {
        let _serial = PROBE_PARK_TEST_LOCK.lock().await;
        const TRACK: &str = "probe-park-lost-wakeup-track";
        probe_park::watch(TRACK);
        let _park = ParkGuard;
        // Declared after `_park` so it drops first and frees the waiter
        // from the gap window even when an assertion below panics.
        let _poke = GapPokeGuard;

        probe_park::arm_gap();
        let (waiter_done_tx, waiter_done_rx) = mpsc::channel();
        // The join handle is deliberately unused: completion is observed
        // through the bounded channel below so a stuck waiter fails the
        // test instead of hanging `join`.
        let _waiter = thread::spawn(move || {
            if let Some(guard) = probe_park::enter_if_watched(TRACK) {
                guard.wait_for_release();
            }
            waiter_done_tx.send(()).expect("report waiter completion");
        });

        // The waiter is now parked inside the predicate-to-wait window,
        // holding the wait mutex.
        assert!(
            probe_park::wait_for_gap_occupation(Duration::from_secs(2)),
            "the armed waiter never reached the predicate-to-wait window"
        );

        // Release from a separate thread. With the synchronization fix it
        // may only complete after the waiter has registered on the condvar.
        let (releaser_entered_tx, releaser_entered_rx) = mpsc::channel();
        let (releaser_done_tx, releaser_done_rx) = mpsc::channel();
        let releaser = thread::spawn(move || {
            releaser_entered_tx.send(()).expect("report release entry");
            probe_park::release();
            releaser_done_tx
                .send(())
                .expect("report release completion");
        });
        releaser_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the releaser never reached release()");
        assert!(
            releaser_done_rx.try_recv().is_err(),
            "release() completed while a waiter still held the predicate-to-wait window"
        );

        // Let the waiter leave the window and register on the condvar. The
        // blocked release must then complete and wake the parked probe.
        probe_park::poke_gap();
        waiter_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the parked probe must wake after the gap release");
        releaser_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the blocked release never completed after the waiter registered");
        releaser.join().expect("releaser thread panicked");
        assert_eq!(
            probe_park::in_flight(),
            0,
            "the released probe must leave the gate"
        );
    }
}
